//! Codex compaction queued for the next true idle point (contract 1.47.0; follow-up 1.47.1).
//!
//! `api session compact --when-idle` journals one pending compaction per session. The supervisor
//! fires `thread/compact/start` on the account app-server once the thread has no active turn
//! and an empty native queue, under the same session ownership every send takes, so no input
//! can be started between the check and the start. While the operation is pending a Codex
//! session takes no new native input: sends stay local (`pending_local`, reason
//! `compaction_hold`) and are delivered, in arrival order and through the ordinary send path,
//! once the operation completes, fails, expires or is cancelled. Nothing here types into a TUI.
//!
//! Lock order is the messaging one: session ownership first, then the root-wide ledger lease.
//! The start needs no ledger at all; releasing held input waits for the ledger (`open_wait`)
//! only to read ids and never holds it across provider I/O (each delivery is an ordinary
//! `send_public` of the original request, which is idempotent per message id).
//!
//! Contract 1.64.0 (SM6): the same request compacts a Claude terminal session (`/compact` typed
//! at the next idle point, [`claude`]); Antigravity and Gemini refuse with
//! `UNSUPPORTED_PROVIDER_OPERATION` (`supported:false`, `reason`). A conversation with nothing
//! (new) to compact completes at once with `nothing_to_compact` / `already_compacted`. Every
//! result has the same keys.
use super::*;
use crate::provider::compact::Method;
use crate::record::{IdleOp, Journal, release};
use anyhow::{Context, ensure};
use futures_util::FutureExt;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub(crate) mod claude;

/// Waiting attempts the app-server may refuse before the operation fails.
const MAX_ATTEMPTS: u32 = 3;
/// Default and maximum lifetime of a pending (not yet started) compaction.
pub const DEFAULT_EXPIRY_SECONDS: i64 = 3600;
pub const MAX_EXPIRY_SECONDS: i64 = 86400;
/// A started compaction that has not finished after this long is reported failed.
const RUN_LIMIT_SECONDS: i64 = 1800;
/// A compaction that never shows up as a turn although the thread is idle again.
const OBSERVE_GRACE_SECONDS: i64 = 30;
/// A started compaction with no observable progress this long (the thread cannot be read, or it
/// stays non-idle with no new turn that could be the compaction) fails `compaction_not_observed`
/// and releases the hold (contract 1.47.1). A running compaction turn counts as progress, so a long
/// compaction is bounded by `RUN_LIMIT_SECONDS` only.
const UNOBSERVED_LIMIT_SECONDS: i64 = 180;
/// At most one daemon-log line per operation and reason this often.
const LOG_EVERY_SECONDS: i64 = 60;
/// Held messages delivered per release pass; the rest wait for the next tick.
const MAX_RELEASE_PER_PASS: usize = 20;
/// A just-released operation is re-swept this long for input that was held at the last moment.
const STRAY_SWEEP_SECONDS: i64 = 60;
pub const POLL_SECONDS: u64 = 2;
/// The phases of a started compaction.
const RUN: &[&str] = &["starting", "running"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    schema_version: u32,
    id: String,
    session_id: String,
    accepted_generation: String,
    conversation_id: String,
    /// Native runtime generation at request time; a restarted session fails the operation.
    runtime_generation: String,
    /// waiting | starting | running | complete | failed | cancelled | expired
    phase: String,
    /// Why a waiting operation has not started yet, or why a finished one ended as it did.
    reason: Option<String>,
    created_at: i64,
    updated_at: i64,
    expires_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    attempts: u32,
    /// Latest turn ids before the start; the compaction turn is the first one not in it.
    baseline: Vec<String>,
    compaction_turn_id: Option<String>,
    #[serde(default)]
    hold_released: bool,
    #[serde(default)]
    release_claimed_at: Option<i64>,
    /// `codex` (every record before 1.64.0) or `claude` (contract 1.64.0).
    #[serde(default = "codex")]
    provider: String,
    #[serde(default)]
    account: Option<String>,
}
fn codex() -> String {
    "codex".into()
}
impl Record {
    fn method(&self) -> Method {
        crate::provider::compact::method_of(&self.provider)
    }
}

crate::record::idle_op!(Record, COMPACTIONS, COMPACTIONS_LOCK);

/// Admission shared by both idle-point operations: a valid `--operation` id and an expiry
/// within `MAX_EXPIRY_SECONDS`.
pub(super) fn admission(operation: Option<&str>, expires_at: i64, now: i64) -> anyhow::Result<()> {
    if let Some(id) = operation {
        uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    }
    ensure!(
        expires_at > now && expires_at <= now + MAX_EXPIRY_SECONDS,
        "INVALID_EXPIRY: expires_at must be within the next {MAX_EXPIRY_SECONDS} seconds"
    );
    Ok(())
}
/// The running Codex app-server of a session: exactly the process (pid and start time) the
/// catalog attachment names, else `STALE_TARGET`.
pub(super) fn codex_target(
    paths: &Paths,
    s: &crate::catalog::Session,
) -> anyhow::Result<Attachment> {
    let attached = attachment(paths, &s.conversation_id).map_err(|e| match e.0 {
        "unavailable" => anyhow::anyhow!("SESSION_NOT_RUNNING"),
        other => anyhow::anyhow!("{}", other.to_uppercase()),
    })?;
    let same =
        |a: &crate::catalog::Attachment| a.pid == attached.pid && a.process_start == attached.start;
    ensure!(s.attachment.as_ref().is_some_and(same), "STALE_TARGET");
    Ok(attached)
}
/// A pending (or the `--operation`) record returned again instead of a second one.
pub(super) fn existing(mut v: Value, existing: bool) -> Value {
    v["existing"] = json!(existing);
    v
}
/// Owner switch and explicit stop: why a waiting Codex operation may not start now.
pub(super) fn codex_blocked(paths: &Paths, conversation: &str) -> Option<&'static str> {
    if !modes::queue::sending_enabled(paths).unwrap_or(false) {
        return Some("native_sending_disabled");
    }
    matches!(
        external::messaging_stop_intent(paths, conversation),
        Ok(Some(_))
    )
    .then_some("explicit_resume_required")
}
/// The thread is at an idle point: no active turn (a failed or interrupted last turn leaves a
/// live session; only a running turn or one waiting on the operator does not) and an empty
/// native queue (input accepted before the request runs first; new input is held meanwhile, so
/// the queue drains and this passes). `Err` is the waiting reason.
pub(super) async fn codex_idle(
    control: &mut Transport,
    conversation: &str,
) -> std::result::Result<(), &'static str> {
    let observed = modes::session_status::observe_fresh_public(control, conversation)
        .await
        .map_err(|_| "provider_unavailable")?;
    if let Some(reason) = observed.reason {
        return Err(reason);
    }
    match observed.state.as_str() {
        "idle" | "errored" | "interrupted" => {}
        "working" => return Err("turn_active"),
        "disconnected" => return Err("session_not_running"),
        _ => return Err("state_unverified"),
    }
    let queue = json!({"threadId":conversation,"cursor":null,"limit":1});
    match control
        .request::<QueuePage>("thread/queue/list", queue)
        .await
    {
        Ok(page) if page.data.is_empty() => Ok(()),
        Ok(_) => Err("native_queue_not_empty"),
        Err(_) => Err("provider_unavailable"),
    }
}
/// A supervisor loop over `pass` every `env` seconds (default `POLL_SECONDS`; 0 disables).
pub(super) async fn supervise<F: std::future::Future<Output = ()>>(
    env: &str,
    pass: impl Fn() -> F,
) {
    let every = std::env::var(env)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(POLL_SECONDS);
    if every == 0 {
        return;
    }
    loop {
        pass().await;
        tokio::time::sleep(crate::timing::secs(every)).await;
    }
}
/// One supervisor pass; a panic in it is logged and must not end the loop for good.
pub(super) async fn guarded(name: &str, pass: impl std::future::Future<Output = ()>) {
    if let Err(panic) = std::panic::AssertUnwindSafe(pass).catch_unwind().await {
        let text = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown");
        eprintln!("{name} monitor: a pass panicked and will be retried: {text}");
    }
}

/// True while new native input for this conversation must be held: a compaction is pending (not
/// past its expiry), starting or running, or it finished but its held input has not been fully
/// released yet (so a new send cannot overtake the held ones). Reads the journal only.
pub fn hold_active(paths: &Paths, conversation: &str) -> bool {
    let now = crate::daemon::now();
    Record::list(paths).is_ok_and(|all| {
        all.iter().any(|r| {
            r.conversation_id == conversation
                && r.method().holds_input()
                && match r.phase.as_str() {
                    "waiting" => now < r.expires_at,
                    "starting" | "running" => true,
                    _ => !r.hold_released,
                }
        })
    })
}

fn explain(reason: &str) -> &'static str {
    match reason {
        "model_change_pending" => "A model change requested earlier is applied first.",
        "turn_active" => "A turn is active; the compaction starts when it ends.",
        "native_queue_not_empty" => "Input already accepted into the native queue runs first.",
        "approval_or_user_input_required" => "The session waits for an approval or an answer.",
        "session_not_running" => "The session has no live app-server attachment.",
        "session_busy" => "Another operation holds the session; retried at the next check.",
        "native_sending_disabled" => "Native sending is disabled by the owner switch.",
        "explicit_resume_required" => "The session was stopped and needs an explicit Resume.",
        "provider_unavailable" => "The account app-server could not be reached; retried.",
        "state_unverified" => "The thread state could not be verified; retried.",
        "provider_rejected" => "The app-server refused thread/compact/start repeatedly.",
        "generation_changed" => "The session restarted after the request; nothing was compacted.",
        "unsupported_native_version" => "The account app-server version is not supported.",
        "compaction_failed" => "The compaction turn failed.",
        "compaction_interrupted" => "The compaction turn was interrupted.",
        "compaction_not_observed" => "No compaction turn was recorded after the start.",
        "compaction_timeout" => "The compaction did not finish within 30 minutes.",
        "expired" => "The expiry passed before the session was idle.",
        "cancelled" => "Cancelled before it started.",
        "completed" => "The compaction turn completed.",
        "nothing_to_compact" => {
            "The conversation has no instruction or reply yet; nothing was started."
        }
        "already_compacted" => "Nothing happened since the last compaction; nothing was started.",
        "draft_present" => "The prompt holds a draft; the compaction waits until it is empty.",
        "terminal_delivery_active" => {
            "A terminal delivery is in progress; retried at the next check."
        }
        "compacted" => "Claude reported the compaction.",
        "start_interrupted" => {
            "The pass typing /compact stopped before it could confirm; not retyped."
        }
        _ => "",
    }
}
fn public(r: &Record) -> Value {
    let next = match r.phase.as_str() {
        "waiting" => "waits for an idle point; api cancel-operation ID cancels it",
        "starting" | "running" => "in progress; cannot be cancelled once started",
        _ => "none",
    };
    let method = r.method();
    let holds = method.holds_input();
    json!({"id":r.id,"kind":"compaction","provider":r.provider,"when":"idle",
        // Contract 1.64.0: one shape for every provider.
        "supported":true,"method":method.name(),"native_method":method.native(),
        "already_compacted":r.reason.as_deref() == Some("already_compacted"),
        "nothing_to_compact":r.reason.as_deref() == Some("nothing_to_compact"),
        "session_id":r.session_id,"source_session_id":r.session_id,"accepted_generation":r.accepted_generation,
        "provider_conversation_id":r.conversation_id,"phase":r.phase,"terminal":r.done(),
        "reason":r.reason,"detail":r.reason.as_deref().map(explain).filter(|s| !s.is_empty()),
        "hold":{"active":holds && !r.done(),"released":r.hold_released,
            "scope":if holds {json!("new public submit and queue sends to this session")} else {Value::Null}},
        "created_at":r.created_at,"updated_at":r.updated_at,"expires_at":r.expires_at,
        "started_at":r.started_at,"finished_at":r.finished_at,"attempts":r.attempts,
        "compaction_turn_id":r.compaction_turn_id,
        "can_cancel":r.phase == "waiting","can_respond":false,"can_recover":false,
        "non_steering":true,"replays":false,"next":next})
}

pub struct Request<'a> {
    pub session: &'a str,
    pub expected_generation: &'a str,
    pub expires_at: Option<i64>,
    pub operation: Option<&'a str>,
}

/// Queue a compaction for the next idle point. One pending compaction per session: asking
/// again returns the pending one (`existing: true`) and never creates a second.
pub fn request(paths: &Paths, req: Request) -> anyhow::Result<Value> {
    let session = crate::catalog::resolve(paths, req.session, req.expected_generation)?;
    let provider = crate::provider::ProviderId::parse(&session.provider)?;
    let method = crate::provider::compact::method(provider);
    if let Method::Unsupported(reason) = method {
        return Err(crate::provider::stop_shape::Unsupported(reason).into());
    }
    let now = crate::daemon::now();
    let expires_at = req.expires_at.unwrap_or(now + DEFAULT_EXPIRY_SECONDS);
    admission(req.operation, expires_at, now)?;
    let (runtime_generation, home) = if method == Method::Terminal {
        claude::admit(paths, &session)?
    } else {
        let attached = codex_target(paths, &session)?;
        (attached.generation(), attached.home.clone())
    };
    let _lock = Record::lock(paths)?;
    let all = Record::list(paths)?;
    if let Some(id) = req.operation
        && let Some(r) = all.iter().find(|r| r.id == id)
    {
        ensure!(
            r.conversation_id == session.conversation_id,
            "OPERATION_ID_CONFLICT"
        );
        return Ok(existing(public(r), true));
    }
    if let Some(r) = all
        .iter()
        .find(|r| !r.done() && r.conversation_id == session.conversation_id)
    {
        return Ok(existing(public(r), true));
    }
    // Contract 1.64.0: nothing (new) to compact completes at once; the session is not touched.
    let already = crate::provider::compact::already(provider, &home, &session.conversation_id);
    let r = Record {
        schema_version: 1,
        id: req
            .operation
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned),
        session_id: session.id.clone(),
        accepted_generation: req.expected_generation.to_owned(),
        conversation_id: session.conversation_id.clone(),
        runtime_generation,
        phase: if already.is_some() {
            "complete"
        } else {
            "waiting"
        }
        .into(),
        reason: already.map(|a| a.reason().to_owned()),
        created_at: now,
        updated_at: now,
        expires_at,
        started_at: None,
        finished_at: already.map(|_| now),
        attempts: 0,
        baseline: vec![],
        compaction_turn_id: None,
        hold_released: already.is_some() || !method.holds_input(),
        release_claimed_at: None,
        provider: session.provider.clone(),
        account: Some(session.account.clone()),
    };
    r.write(paths)?;
    Ok(existing(public(&r), false))
}

/// Held message ids of a conversation, in arrival order (best effort; `None` when the
/// ledger is unavailable).
pub(super) async fn held_ids(paths: &Paths, c: &str) -> Option<Vec<String>> {
    let ledger = ledger::open_read(paths, wait_budget(paths, Wait::Read))
        .await
        .ok()?;
    let ids = ledger.held_ids(c, false).ok()?;
    // 1.71.1: released before the journal directory is read.
    drop(ledger);
    Some([ids, claude_steer::held_ids(paths, c, false)].concat())
}

pub async fn status(
    paths: &Paths,
    operation: Option<&str>,
    session: Option<&str>,
) -> anyhow::Result<Value> {
    let r = Record::find(paths, operation, session)?;
    let mut v = public(&r);
    v["held_messages"] = match held_ids(paths, &r.conversation_id).await {
        Some(ids) => json!(ids),
        None => Value::Null,
    };
    Ok(v)
}

/// Cancel a compaction that has not started; refused once the start was attempted (the
/// app-server cannot take it back) or after it finished.
pub async fn cancel(paths: &Paths, id: &str) -> anyhow::Result<Value> {
    let started = "COMPACTION_ALREADY_STARTED";
    let (r, requested) = Record::cancel(paths, id, started, "COMPACTION_ALREADY_FINISHED").await?;
    let mut v = public(&r);
    if requested {
        v["cancellation_requested"] = json!(true);
    }
    Ok(v)
}

pub fn exists(paths: &Paths, id: &str) -> bool {
    Record::exists(paths, id)
}
pub fn operations(paths: &Paths) -> anyhow::Result<Vec<Value>> {
    Ok(Record::rows(paths)?.iter().map(public).collect())
}

/// Deliver the input held for a finished compaction, once. Every message goes through the
/// ordinary public send of its original request (same id, same bytes), which settles a
/// concurrent delivery of the same id to exactly one native add.
/// A transient failure left this message `pending_local` with some delivery error as its reason:
/// mark it so the next release pass finds it again. The message's own expiry bounds the retries.
async fn mark_retry(paths: &Paths, id: &str) {
    let Ok(mut ledger) = ledger::open_write(paths, wait_budget(paths, Wait::Read)).await else {
        return;
    };
    if let Ok(Some(mut receipt)) = ledger.get(id)
        && receipt.state == "pending_local"
        && receipt.reason.as_deref() != Some("compaction_release_retry")
    {
        receipt.reason = Some("compaction_release_retry".into());
        let _ = ledger.save(&receipt);
    }
}

/// Deliver the input held for a conversation, in arrival order, each through the ordinary
/// idempotent public send with queue semantics (`send_released`). Returns true only when nothing
/// is left to retry: a ledger or delivery failure (including a receipt that is still
/// `pending_local` afterwards) stops the pass at that message, so later ones never overtake it,
/// and returns false so the next tick re-queries and retries. A pass handles at most
/// `MAX_RELEASE_PER_PASS` messages; input that arrived while releasing is picked up by the next round.
pub(crate) async fn deliver_held(paths: &Paths, conversation: &str) -> bool {
    // Contract 1.49.0: held input is released once, after every idle-point operation of the
    // conversation (compactions and model changes) has ended.
    if Record::pending_before(paths, conversation, None)
        || super::model_change::Record::pending_before(paths, conversation, None)
    {
        return false;
    }
    for _ in 0..3 {
        let Some(ids) = held_ids(paths, conversation).await else {
            return false;
        };
        if ids.is_empty() {
            return true;
        }
        let more = ids.len() > MAX_RELEASE_PER_PASS;
        for message in ids.into_iter().take(MAX_RELEASE_PER_PASS) {
            match public::send_released(paths, &message).await {
                Ok(v) if v["state"] != "pending_local" => {}
                _ => {
                    mark_retry(paths, &message).await;
                    return false;
                }
            }
        }
        if more {
            return false;
        }
    }
    false
}

#[derive(Deserialize)]
struct TurnsPage {
    data: Vec<CompactionTurn>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}
#[derive(Clone, Deserialize)]
struct CompactionTurn {
    id: String,
    status: String,
    #[serde(default)]
    items: Vec<ItemKind>,
}
#[derive(Clone, Deserialize)]
struct ItemKind {
    #[serde(rename = "type")]
    kind: String,
}
/// The item type of a compaction (app-server v2 `ThreadItem`, `ContextCompactionThreadItem`).
const COMPACTION_ITEM: &str = "contextCompaction";
impl CompactionTurn {
    fn has(&self, kind: &str) -> bool {
        self.items.iter().any(|i| i.kind == kind)
    }
}
/// The compaction turn among the turns that were not there before the start: one that carries a
/// positive `contextCompaction` item. A turn whose summary shows a user message is somebody
/// else's. Never inferred from the absence of a user message.
///
/// Real app-servers (0.163-0.166) list a compaction turn in the summary view with `items: []`
/// (the summary keeps only user and agent messages), and refuse `thread/items/list` with -32601
/// for legacy-history threads. So every other new turn is read once in the full view, exactly
/// that turn (contract 1.47.1). The `contextCompaction` item appears there when it completes.
async fn compaction_turn(
    control: &mut Transport,
    conversation: &str,
    turns: &[CompactionTurn],
    baseline: &[String],
) -> Result<Option<CompactionTurn>> {
    for (index, turn) in turns.iter().enumerate() {
        if baseline.contains(&turn.id) {
            continue;
        }
        if turn.has(COMPACTION_ITEM) {
            return Ok(Some(turn.clone()));
        }
        if turn.has("userMessage") {
            continue;
        }
        if let Some(full) = full_turn(control, conversation, turns, index).await?
            && full.has(COMPACTION_ITEM)
        {
            return Ok(Some(full));
        }
    }
    Ok(None)
}
/// Read `turns[index]` (of a newest-first summary page) in the full view. Positions it with the
/// server's own cursor after the turn before it; a different turn there (the thread moved on
/// between the reads) is `None`, retried on the next pass.
async fn full_turn(
    control: &mut Transport,
    conversation: &str,
    turns: &[CompactionTurn],
    index: usize,
) -> Result<Option<CompactionTurn>> {
    let cursor = if index == 0 {
        None
    } else {
        let before: TurnsPage = control
            .passive_summary_page(conversation, None, index as u32)
            .await?;
        if before.data.last().map(|t| t.id.as_str()) != Some(turns[index - 1].id.as_str()) {
            return Ok(None);
        }
        match before.next_cursor {
            Some(cursor) => Some(cursor),
            None => return Ok(None),
        }
    };
    let page: TurnsPage = control
        .compaction_turn_page(conversation, cursor.as_deref())
        .await?;
    Ok(page
        .data
        .into_iter()
        .next()
        .filter(|t| t.id == turns[index].id))
}
async fn recent_turns(control: &mut Transport, conversation: &str) -> Result<Vec<CompactionTurn>> {
    let page: TurnsPage = control.passive_summary_page(conversation, None, 5).await?;
    Ok(page.data)
}

/// One supervisor pass over every journaled compaction.
pub async fn tick(paths: &Paths) {
    let Ok(all) = Record::list(paths) else { return };
    // Starts and follow-ups first, so a release backlog cannot stall another pending compaction.
    prune(
        &all.iter()
            .filter(|r| !r.done())
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
    );
    for r in all.iter().filter(|r| !r.done()) {
        if r.method() == Method::Terminal {
            claude::advance(paths, &r.id).await;
        } else {
            advance(paths, &r.id).await;
        }
    }
    let now = crate::daemon::now();
    let Ok(all) = Record::list(paths) else { return };
    for r in all.iter().filter(|r| r.done() && r.method().holds_input()) {
        if !r.hold_released {
            release::<Record>(paths, &r.id).await;
        } else if r
            .finished_at
            .is_some_and(|at| now - at < STRAY_SWEEP_SECONDS)
        {
            // Input held at the very last moment, after the final query of the release.
            deliver_held(paths, &r.conversation_id).await;
        }
    }
}

async fn advance(paths: &Paths, id: &str) {
    let Ok(r) = Record::read(paths, id) else {
        return;
    };
    let now = crate::daemon::now();
    match r.phase.as_str() {
        "waiting" if now >= r.expires_at => {
            Record::finish(paths, id, &["waiting"], "expired", "expired")
        }
        "waiting" => try_start(paths, r).await,
        "starting" | "running" => follow(paths, r).await,
        _ => {}
    }
}

/// Fire the compaction when the thread is truly idle. Provider I/O runs under session
/// ownership only, never under the ledger lease.
async fn try_start(paths: &Paths, r: Record) {
    let id = r.id.clone();
    let conversation = r.conversation_id.clone();
    // Contract 1.49.0: idle-point operations of one conversation run in request order.
    if super::model_change::Record::pending_before(
        paths,
        &conversation,
        Some((r.created_at, &r.id)),
    ) {
        return Record::waiting(paths, &id, "model_change_pending");
    }
    let attached = match attachment(paths, &conversation) {
        Ok(a) => a,
        Err(Fault("unavailable")) => return Record::waiting(paths, &id, "session_not_running"),
        Err(e) => return Record::waiting(paths, &id, e.0),
    };
    if attached.generation() != r.runtime_generation {
        return Record::finish(paths, &id, &["waiting"], "failed", "generation_changed");
    }
    if let Some(why) = codex_blocked(paths, &conversation) {
        return Record::waiting(paths, &id, why);
    }
    let budget = std::time::Duration::from_secs(3);
    let _guard = match mutation_lock_waiting(paths, &conversation, budget).await {
        Ok(g) => g,
        Err(_) => return Record::waiting(paths, &id, "session_busy"),
    };
    let mut control = match attached.connect().await {
        Ok(c) => c,
        Err(Fault("unsupported_native_version" | "unsupported")) => {
            return Record::finish(
                paths,
                &id,
                &["waiting"],
                "failed",
                "unsupported_native_version",
            );
        }
        Err(_) => return Record::waiting(paths, &id, "provider_unavailable"),
    };
    if let Err(why) = codex_idle(&mut control, &conversation).await {
        return Record::waiting(paths, &id, why);
    }
    let baseline = match recent_turns(&mut control, &conversation).await {
        Ok(turns) => turns.into_iter().map(|t| t.id).collect::<Vec<_>>(),
        Err(_) => return Record::waiting(paths, &id, "provider_unavailable"),
    };
    if attachment(paths, &conversation)
        .map(|a| a.generation())
        .ok()
        .as_deref()
        != Some(r.runtime_generation.as_str())
    {
        return Record::finish(paths, &id, &["waiting"], "failed", "generation_changed");
    }
    // Journal the intent before touching the app-server. Lost to a cancel or expiry: stop.
    let started = Record::update(paths, &id, |r| {
        if r.phase != "waiting" || crate::daemon::now() >= r.expires_at {
            return false;
        }
        r.phase = "starting".into();
        r.reason = None;
        r.attempts += 1;
        r.baseline = baseline.clone();
        true
    });
    let Ok(Some(started)) = started else { return };
    match control.compact_start(&conversation).await {
        Ok(()) => {
            let _ = Record::update(paths, &id, |r| {
                if r.phase != "starting" {
                    return false;
                }
                r.phase = "running".into();
                r.started_at = Some(crate::daemon::now());
                true
            });
        }
        Err(Fault("native_rejected")) => {
            // Definitively not started (the app-server answered with an error), for example
            // a task or background work that still counts as in progress. Wait and retry.
            let refused = started.attempts >= MAX_ATTEMPTS;
            let _ = Record::update(paths, &id, |r| {
                if r.phase != "starting" {
                    return false;
                }
                if refused {
                    r.phase = "failed".into();
                    r.reason = Some("provider_rejected".into());
                    r.finished_at = Some(crate::daemon::now());
                } else {
                    r.phase = "waiting".into();
                    r.reason = Some("provider_rejected".into());
                }
                true
            });
        }
        // Timeout or a dropped connection: unknown whether it started. `follow` decides from
        // the thread's own turns; it never restarts on a guess.
        Err(_) => {}
    }
}

/// Last pass (unix seconds) that saw a started compaction make observable progress: the thread
/// was read and was idle, or a new turn that may be the compaction was still running. Process
/// memory only; after a supervisor restart the start time counts instead.
static PROGRESS: Mutex<BTreeMap<String, i64>> = Mutex::new(BTreeMap::new());
/// Last daemon-log line per operation and reason, for `LOG_EVERY_SECONDS`.
static LOGGED: Mutex<BTreeMap<(String, &'static str), i64>> = Mutex::new(BTreeMap::new());

fn progressed(id: &str, now: i64) {
    if let Ok(mut map) = PROGRESS.lock() {
        map.insert(id.to_owned(), now);
    }
}
fn last_progress(id: &str, since: i64) -> i64 {
    PROGRESS
        .lock()
        .ok()
        .and_then(|map| map.get(id).copied())
        .map_or(since, |at| at.max(since))
}
/// Forget the in-memory state of operations that are no longer followed.
fn prune(live: &[&str]) {
    if let Ok(mut map) = PROGRESS.lock() {
        map.retain(|id, _| live.contains(&id.as_str()));
    }
    if let Ok(mut map) = LOGGED.lock() {
        map.retain(|(id, _), _| live.contains(&id.as_str()));
    }
}
/// Whether a line for this operation and reason is due (at most one per `LOG_EVERY_SECONDS`).
fn log_due(id: &str, reason: &'static str, now: i64) -> bool {
    let Ok(mut map) = LOGGED.lock() else {
        return true;
    };
    let key = (id.to_owned(), reason);
    match map.get(&key) {
        Some(at) if now - at < LOG_EVERY_SECONDS => false,
        _ => {
            map.insert(key, now);
            true
        }
    }
}
/// One rate-limited daemon-log line (the supervisor's stderr is `logs/daemon.log`).
fn note(id: &str, reason: &'static str, detail: &str, now: i64, since: i64) {
    if log_due(id, reason, now) {
        eprintln!(
            "compaction {id}: not observed this pass: {reason} ({detail}); started {}s ago",
            now - since
        );
    }
}

/// What one pass learned about a started compaction.
struct Observation {
    activity: &'static str,
    found: Option<CompactionTurn>,
    /// A new turn without a user message is still running: most likely the compaction itself.
    compacting: bool,
}
async fn observe(
    attached: &Attachment,
    conversation: &str,
    baseline: &[String],
) -> std::result::Result<Observation, (&'static str, Fault)> {
    let mut control = attached.connect().await.map_err(|e| ("connect", e))?;
    let (activity, kind) = activity_and_kind(&mut control, conversation)
        .await
        .map_err(|e| ("thread_read", e))?;
    // `systemError` (a failed turn, a usage limit) runs nothing: it is quiescent like idle, not the
    // busy thread the log used to call "active".
    let activity = if kind == "systemError" {
        "errored"
    } else {
        activity
    };
    let turns = recent_turns(&mut control, conversation)
        .await
        .map_err(|e| ("turns_list", e))?;
    let found = compaction_turn(&mut control, conversation, &turns, baseline)
        .await
        .map_err(|e| ("turn_read", e))?;
    let compacting = turns
        .iter()
        .any(|t| !baseline.contains(&t.id) && !t.has("userMessage") && t.status == "inProgress");
    Ok(Observation {
        activity,
        found,
        compacting,
    })
}

/// Track a started (or possibly started) compaction to its end. Every pass that cannot decide
/// says why in the daemon log (rate-limited), and a compaction that stays unobservable is failed
/// after `UNOBSERVED_LIMIT_SECONDS` instead of holding input until `RUN_LIMIT_SECONDS`.
async fn follow(paths: &Paths, r: Record) {
    let id = r.id.clone();
    let conversation = r.conversation_id.clone();
    let now = crate::daemon::now();
    let attached = match attachment(paths, &conversation) {
        Ok(a) if a.generation() == r.runtime_generation => Some(a),
        Ok(_) => {
            return Record::finish(paths, &id, RUN, "failed", "generation_changed");
        }
        Err(_) => None,
    };
    let since = r.started_at.unwrap_or(r.updated_at);
    if now - since > RUN_LIMIT_SECONDS {
        return Record::finish(paths, &id, RUN, "failed", "compaction_timeout");
    }
    let Some(attached) = attached else {
        // No attachment: the process is gone. A starting one never started; a running one
        // cannot be followed.
        return Record::finish(paths, &id, RUN, "failed", "compaction_not_observed");
    };
    let observed = match observe(&attached, &conversation, &r.baseline).await {
        Ok(o) => o,
        Err((step, fault)) => {
            note(&id, step, fault.0, now, since);
            return give_up_if_unobserved(paths, &id, now, since);
        }
    };
    match observed.found {
        Some(turn) => {
            progressed(&id, now);
            let turn_id = turn.id.clone();
            match turn.status.as_str() {
                "completed" => {
                    let done = Record::update(paths, &id, |r| {
                        if !matches!(r.phase.as_str(), "starting" | "running") {
                            return false;
                        }
                        r.phase = "complete".into();
                        r.reason = Some("completed".into());
                        r.compaction_turn_id = Some(turn_id);
                        r.started_at.get_or_insert(now);
                        r.finished_at = Some(now);
                        true
                    });
                    // Contract 1.64.0: the compaction is a session event too.
                    if let Ok(Some(r)) = done {
                        let _ = crate::provider::events::record(
                            paths,
                            crate::provider::ProviderId::Codex,
                            &r.session_id,
                            &r.conversation_id,
                            "compacted",
                            json!({"operation_id":r.id,"trigger":"sessionsation"}),
                        );
                    }
                }
                "failed" | "interrupted" => {
                    let reason = if turn.status == "failed" {
                        "compaction_failed"
                    } else {
                        "compaction_interrupted"
                    };
                    let _ = Record::update(paths, &id, |r| {
                        if !matches!(r.phase.as_str(), "starting" | "running") {
                            return false;
                        }
                        r.phase = "failed".into();
                        r.reason = Some(reason.into());
                        r.compaction_turn_id = Some(turn_id);
                        r.finished_at = Some(now);
                        true
                    });
                }
                _ => {
                    let _ = Record::update(paths, &id, |r| {
                        if !matches!(r.phase.as_str(), "starting" | "running") {
                            return false;
                        }
                        r.phase = "running".into();
                        r.started_at.get_or_insert(now);
                        r.compaction_turn_id = Some(turn_id);
                        true
                    });
                }
            }
        }
        // Idle again and no compaction turn after the window: report it, release the hold. A start
        // that was sent is never sent again (a slow app-server could compact twice).
        None if matches!(observed.activity, "idle" | "errored")
            && now - since > OBSERVE_GRACE_SECONDS =>
        {
            note(
                &id,
                "idle_without_compaction_turn",
                &format!("thread {}, failing", observed.activity),
                now,
                since,
            );
            Record::finish(paths, &id, RUN, "failed", "compaction_not_observed");
        }
        None if matches!(observed.activity, "idle" | "errored") || observed.compacting => {
            progressed(&id, now)
        }
        // Not idle and no new turn that could be the compaction (for example background work
        // keeps the thread active, or another status): bounded, never silent.
        None => {
            note(
                &id,
                "no_compaction_turn",
                &format!("thread {}", observed.activity),
                now,
                since,
            );
            give_up_if_unobserved(paths, &id, now, since);
        }
    }
}

/// Fail `compaction_not_observed` (which releases the hold) once no pass has seen progress for
/// `UNOBSERVED_LIMIT_SECONDS`.
fn give_up_if_unobserved(paths: &Paths, id: &str, now: i64, since: i64) {
    let last = last_progress(id, since);
    if now - last <= UNOBSERVED_LIMIT_SECONDS {
        return;
    }
    eprintln!(
        "compaction {id}: no observable progress for {}s (started {}s ago); failing compaction_not_observed and releasing the hold",
        now - last,
        now - since
    );
    Record::finish(paths, id, RUN, "failed", "compaction_not_observed");
}

/// Supervisor loop (`SESSIONSATION_COMPACTION_SECONDS`, 0 disables).
pub async fn monitor(paths: Paths) {
    supervise("SESSIONSATION_COMPACTION_SECONDS", || {
        guarded("compaction", tick(&paths))
    })
    .await
}

/// `api capabilities.session_compact`.
pub fn capability() -> Value {
    json!({"since_contract":"1.47.0","command":"session compact","provider":["codex","claude"],
        // Contract 1.64.0 (SM6): one request and result shape for every provider.
        "unsupported":{"gemini":crate::provider::compact::UNSUPPORTED_CODE,"antigravity":crate::provider::compact::UNSUPPORTED_CODE},
        "providers":{"codex":{"supported":true,"method":"app_server","native_method":"thread/compact/start","holds_input":true},
            "claude":{"supported":true,"method":"terminal","native_method":claude::COMMAND,"holds_input":true,
                "requires":["herdr_terminal","terminal_input_enabled","idle","empty_prompt"],
                "completed_by":["PostCompact hook event","transcript compact_boundary"],
                "hosted":"refused: a hosted session has no terminal pane (TERMINAL_* code)"},
            "antigravity":{"supported":false,"reason":crate::provider::compact::ANTIGRAVITY_UNSUPPORTED},
            "gemini":{"supported":false,"reason":crate::provider::conversation::GEMINI_UNSUPPORTED}},
        "result_keys_since_1_64":["supported","method","native_method","already_compacted","nothing_to_compact"],
        "already":{"nothing_to_compact":"the history file has no instruction or reply yet","already_compacted":"the newest compaction is newer than the last instruction and reply",
            "phase":"complete","started":false},
        "unsupported_error":{"code":crate::provider::compact::UNSUPPORTED_CODE,"supported":false,"reason":"error.reason"},
        "when":["idle"],"native_method":"thread/compact/start","types_into_terminal":"claude only",
        "pending_per_session":1,"idempotent":"existing pending operation returned with existing:true",
        "default_expiry_seconds":DEFAULT_EXPIRY_SECONDS,"max_expiry_seconds":MAX_EXPIRY_SECONDS,
        "phases":["waiting","starting","running","complete","failed","cancelled","expired"],
        "waiting_reasons":["turn_active","native_queue_not_empty","approval_or_user_input_required","session_not_running","session_busy","native_sending_disabled","explicit_resume_required","provider_unavailable","state_unverified","provider_rejected"],
        "failure_reasons":["generation_changed","provider_rejected","unsupported_native_version","compaction_failed","compaction_interrupted","compaction_not_observed","compaction_timeout"],
        "status":"session compact-status","cancel":"session compact-cancel","cancel_operation":"cancel-operation",
        "cancel_while":["waiting"],"max_attempts":MAX_ATTEMPTS,
        "hold":{"scope":"new public submit and queue sends to the session until the held input is released","not_held":["steer","resume","legacy messaging enqueue","modes"],"held_receipt":{"state":"pending_local","reason":"compaction_hold"},"release_delivery":"queue semantics for every held message, in arrival order","release_retry_reason":"compaction_release_retry",
            "released":"in arrival order after complete, failed, cancelled or expired","already_native_queued":"runs first; the compaction waits for the native queue to drain"},
        "journal":crate::layout::COMPACTIONS,"replays":false})
}

#[cfg(test)]
mod tests;
