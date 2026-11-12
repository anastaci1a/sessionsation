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
use super::*;
use anyhow::{Context, ensure};
use futures_util::FutureExt;
use std::collections::BTreeMap;
use std::sync::Mutex;

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
/// A release claim older than this is taken over (its process died).
const RELEASE_CLAIM_SECONDS: i64 = 120;
pub const POLL_SECONDS: u64 = 2;

const TERMINAL: &[&str] = &["complete", "failed", "cancelled", "expired"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Record {
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
}
impl Record {
    fn terminal(&self) -> bool {
        TERMINAL.contains(&self.phase.as_str())
    }
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::COMPACTIONS)
}
fn file(paths: &Paths, id: &str) -> anyhow::Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|f| f.is_file())
}
fn read(paths: &Paths, id: &str) -> anyhow::Result<Record> {
    let bytes = fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?;
    serde_json::from_slice(&bytes).context("UNREADABLE_OPERATION_RECORD")
}
fn save(paths: &Paths, r: &Record) -> anyhow::Result<()> {
    crate::paths::atomic_write(&file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
fn list(paths: &Paths) -> anyhow::Result<Vec<Record>> {
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
/// Short critical section around journal read-modify-write; never held across an await.
fn lock(paths: &Paths) -> anyhow::Result<fs::File> {
    private_dir(&paths.root.join(crate::layout::OPERATIONS))?;
    let f = lock_file(&paths.root.join(crate::layout::COMPACTIONS_LOCK))?;
    f.lock_exclusive()?;
    Ok(f)
}
/// Apply `edit` to the stored record when it returns true; `None` when it declined. Every
/// transition goes through this so a concurrent cancel or expiry is never overwritten.
fn update(
    paths: &Paths,
    id: &str,
    edit: impl FnOnce(&mut Record) -> bool,
) -> anyhow::Result<Option<Record>> {
    let _lock = lock(paths)?;
    let mut r = read(paths, id)?;
    if !edit(&mut r) {
        return Ok(None);
    }
    r.updated_at = crate::daemon::now();
    save(paths, &r)?;
    Ok(Some(r))
}
fn finish(paths: &Paths, id: &str, from: &[&str], phase: &str, reason: &str) {
    let _ = update(paths, id, |r| {
        if !from.contains(&r.phase.as_str()) {
            return false;
        }
        r.phase = phase.into();
        r.reason = Some(reason.into());
        r.finished_at = Some(crate::daemon::now());
        true
    });
}
fn waiting(paths: &Paths, id: &str, why: &str) {
    let _ = update(paths, id, |r| {
        if r.phase != "waiting" || r.reason.as_deref() == Some(why) {
            return false;
        }
        r.reason = Some(why.into());
        true
    });
}

/// True while new native input for this conversation must be held: a compaction is pending (not
/// past its expiry), starting or running, or it finished but its held input has not been fully
/// released yet (so a new send cannot overtake the held ones). Reads the journal only.
pub fn hold_active(paths: &Paths, conversation: &str) -> bool {
    let now = crate::daemon::now();
    list(paths).is_ok_and(|all| {
        all.iter().any(|r| {
            r.conversation_id == conversation
                && match r.phase.as_str() {
                    "waiting" => now < r.expires_at,
                    "starting" | "running" => true,
                    _ => !r.hold_released,
                }
        })
    })
}

/// Any non-terminal compaction of the conversation (contract 1.49.0 release precondition).
pub(super) fn pending(paths: &Paths, conversation: &str) -> bool {
    list(paths).is_ok_and(|all| {
        all.iter()
            .any(|r| !r.terminal() && r.conversation_id == conversation)
    })
}
/// A non-terminal compaction of the conversation requested before `(created_at, id)`.
pub(super) fn pending_before(paths: &Paths, conversation: &str, key: (i64, &str)) -> bool {
    list(paths).is_ok_and(|all| {
        all.iter().any(|r| {
            !r.terminal()
                && r.conversation_id == conversation
                && (r.created_at, r.id.as_str()) < key
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
        _ => "",
    }
}
fn public(r: &Record) -> Value {
    let next = match r.phase.as_str() {
        "waiting" => "waits for an idle point; api cancel-operation ID cancels it",
        "starting" | "running" => "in progress; cannot be cancelled once started",
        _ => "none",
    };
    json!({"id":r.id,"kind":"compaction","provider":"codex","when":"idle",
        "session_id":r.session_id,"source_session_id":r.session_id,"accepted_generation":r.accepted_generation,
        "provider_conversation_id":r.conversation_id,"phase":r.phase,"terminal":r.terminal(),
        "reason":r.reason,"detail":r.reason.as_deref().map(explain).filter(|s| !s.is_empty()),
        "hold":{"active":!r.terminal(),"released":r.hold_released,"scope":"new native input for this session"},
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
    ensure!(
        session.provider == "codex",
        "COMPACT_UNSUPPORTED: {} sessions cannot be compacted through Sessionsation{}",
        session.provider,
        if session.provider == "claude" {
            " (Claude agents compact themselves with /compact)"
        } else {
            ""
        }
    );
    if let Some(id) = req.operation {
        uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    }
    let now = crate::daemon::now();
    let expires_at = req.expires_at.unwrap_or(now + DEFAULT_EXPIRY_SECONDS);
    ensure!(
        expires_at > now && expires_at <= now + MAX_EXPIRY_SECONDS,
        "INVALID_EXPIRY: expires_at must be within the next {MAX_EXPIRY_SECONDS} seconds"
    );
    let attached = attachment(paths, &session.conversation_id).map_err(|e| match e.0 {
        "unavailable" => anyhow::anyhow!("SESSION_NOT_RUNNING"),
        other => anyhow::anyhow!("{}", other.to_uppercase()),
    })?;
    ensure!(
        session
            .attachment
            .as_ref()
            .is_some_and(|a| a.pid == attached.pid && a.process_start == attached.start),
        "STALE_TARGET"
    );
    let _lock = lock(paths)?;
    let all = list(paths)?;
    if let Some(id) = req.operation
        && let Some(r) = all.iter().find(|r| r.id == id)
    {
        ensure!(
            r.conversation_id == session.conversation_id,
            "OPERATION_ID_CONFLICT"
        );
        let mut v = public(r);
        v["existing"] = json!(true);
        return Ok(v);
    }
    if let Some(r) = all
        .iter()
        .find(|r| !r.terminal() && r.conversation_id == session.conversation_id)
    {
        let mut v = public(r);
        v["existing"] = json!(true);
        return Ok(v);
    }
    let r = Record {
        schema_version: 1,
        id: req
            .operation
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned),
        session_id: session.id.clone(),
        accepted_generation: req.expected_generation.to_owned(),
        conversation_id: session.conversation_id.clone(),
        runtime_generation: attached.generation(),
        phase: "waiting".into(),
        reason: None,
        created_at: now,
        updated_at: now,
        expires_at,
        started_at: None,
        finished_at: None,
        attempts: 0,
        baseline: vec![],
        compaction_turn_id: None,
        hold_released: false,
        release_claimed_at: None,
    };
    save(paths, &r)?;
    let mut v = public(&r);
    v["existing"] = json!(false);
    Ok(v)
}

/// Held message ids of a conversation, in arrival order (best effort; `None` when the
/// ledger is unavailable).
pub(super) async fn held_ids(paths: &Paths, conversation: &str) -> Option<Vec<String>> {
    let ledger = Ledger::open_wait(paths, wait_budget(paths, Wait::Read))
        .await
        .ok()?;
    public::initialize(&ledger).ok()?;
    let mut statement = ledger
        .db
        .prepare(
            "SELECT r.id FROM receipts r JOIN public_messages p ON p.id=r.id \
             WHERE json_extract(r.receipt,'$.state')='pending_local' \
             AND json_extract(r.receipt,'$.reason') IN ('compaction_hold','compaction_release_retry','model_change_hold') \
             AND json_extract(p.binding,'$.native.session_id')=? ORDER BY p.rowid",
        )
        .ok()?;
    let rows = statement
        .query_map([conversation], |row| row.get::<_, String>(0))
        .ok()?;
    Some(rows.flatten().collect())
}

pub async fn status(
    paths: &Paths,
    operation: Option<&str>,
    session: Option<&str>,
) -> anyhow::Result<Value> {
    let r = match (operation, session) {
        (Some(id), _) => read(paths, id)?,
        (None, Some(session)) => {
            let found = crate::catalog::find(paths, session)?;
            let mut mine: Vec<Record> = list(paths)?
                .into_iter()
                .filter(|r| r.conversation_id == found.conversation_id)
                .collect();
            let pick = mine
                .iter()
                .rposition(|r| !r.terminal())
                .or(mine.len().checked_sub(1));
            mine.swap_remove(pick.context("OPERATION_NOT_FOUND")?)
        }
        _ => anyhow::bail!("INVALID_ARGUMENT: --operation or --session is required"),
    };
    let mut v = public(&r);
    v["held_messages"] = match held_ids(paths, &r.conversation_id).await {
        Some(ids) => json!(ids),
        None => Value::Null,
    };
    Ok(v)
}

/// Cancel a compaction that has not started. Idempotent for a cancelled one; refused once the
/// start was attempted (the app-server cannot take it back) or after it finished.
pub async fn cancel(paths: &Paths, id: &str) -> anyhow::Result<Value> {
    let r = read(paths, id)?;
    match r.phase.as_str() {
        "cancelled" => return Ok(public(&r)),
        "waiting" => {}
        "starting" | "running" => anyhow::bail!("COMPACTION_ALREADY_STARTED"),
        _ => anyhow::bail!("COMPACTION_ALREADY_FINISHED"),
    }
    let done = update(paths, id, |r| {
        if r.phase != "waiting" {
            return false;
        }
        r.phase = "cancelled".into();
        r.reason = Some("cancelled".into());
        r.finished_at = Some(crate::daemon::now());
        true
    })?;
    let Some(done) = done else {
        // Lost a race with the start or the expiry: report what actually happened.
        let now = read(paths, id)?;
        anyhow::ensure!(now.phase == "cancelled", "COMPACTION_ALREADY_STARTED");
        return Ok(public(&now));
    };
    release(paths, &done.id).await;
    let mut v = public(&read(paths, id)?);
    v["cancellation_requested"] = json!(true);
    Ok(v)
}

/// `api operations` rows: pending ones and those finished within the last hour.
pub fn operations(paths: &Paths) -> anyhow::Result<Vec<Value>> {
    let now = crate::daemon::now();
    Ok(list(paths)?
        .iter()
        .filter(|r| !r.terminal() || now - r.updated_at < 3600)
        .map(public)
        .collect())
}

/// Deliver the input held for a finished compaction, once. Every message goes through the
/// ordinary public send of its original request (same id, same bytes), which settles a
/// concurrent delivery of the same id to exactly one native add.
/// A transient failure left this message `pending_local` with some delivery error as its reason:
/// mark it so the next release pass finds it again. The message's own expiry bounds the retries.
async fn mark_retry(paths: &Paths, id: &str) {
    let Ok(mut ledger) = Ledger::open_wait(paths, wait_budget(paths, Wait::Read)).await else {
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
pub(super) async fn deliver_held(paths: &Paths, conversation: &str) -> bool {
    // Contract 1.49.0: held input is released once, after every idle-point operation of the
    // conversation (compactions and model changes) has ended.
    if pending(paths, conversation) || super::model_change::pending(paths, conversation) {
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

/// Release the input held for a finished compaction. `hold_released` is set only after a pass
/// found nothing left to deliver or retry; until then the hold stays in force, so a new send
/// cannot overtake held ones, and the next tick tries again. A concurrent release (another
/// process) is skipped by a claim that lapses after `RELEASE_CLAIM_SECONDS`.
async fn release(paths: &Paths, id: &str) {
    let claimed = update(paths, id, |r| {
        let now = crate::daemon::now();
        if !r.terminal()
            || r.hold_released
            || r.release_claimed_at
                .is_some_and(|at| now - at < RELEASE_CLAIM_SECONDS)
        {
            return false;
        }
        r.release_claimed_at = Some(now);
        true
    });
    let Ok(Some(r)) = claimed else { return };
    let done = deliver_held(paths, &r.conversation_id).await;
    let _ = update(paths, id, |r| {
        r.hold_released |= done;
        r.release_claimed_at = None;
        true
    });
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
    let Ok(all) = list(paths) else { return };
    // Starts and follow-ups first, so a release backlog cannot stall another pending compaction.
    prune(
        &all.iter()
            .filter(|r| !r.terminal())
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
    );
    for r in all.iter().filter(|r| !r.terminal()) {
        advance(paths, &r.id).await;
    }
    let now = crate::daemon::now();
    let Ok(all) = list(paths) else { return };
    for r in all.iter().filter(|r| r.terminal()) {
        if !r.hold_released {
            release(paths, &r.id).await;
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
    let Ok(r) = read(paths, id) else { return };
    let now = crate::daemon::now();
    match r.phase.as_str() {
        "waiting" if now >= r.expires_at => finish(paths, id, &["waiting"], "expired", "expired"),
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
    if super::model_change::pending_before(paths, &conversation, (r.created_at, &r.id)) {
        return waiting(paths, &id, "model_change_pending");
    }
    let attached = match attachment(paths, &conversation) {
        Ok(a) => a,
        Err(Fault("unavailable")) => return waiting(paths, &id, "session_not_running"),
        Err(e) => return waiting(paths, &id, e.0),
    };
    if attached.generation() != r.runtime_generation {
        return finish(paths, &id, &["waiting"], "failed", "generation_changed");
    }
    match modes::queue::sending_enabled(paths) {
        Ok(true) => {}
        _ => return waiting(paths, &id, "native_sending_disabled"),
    }
    if matches!(
        external::messaging_stop_intent(paths, &conversation),
        Ok(Some(_))
    ) {
        return waiting(paths, &id, "explicit_resume_required");
    }
    let budget = std::time::Duration::from_secs(3);
    let _guard = match mutation_lock_waiting(paths, &conversation, budget).await {
        Ok(g) => g,
        Err(_) => return waiting(paths, &id, "session_busy"),
    };
    let mut control = match attached.connect().await {
        Ok(c) => c,
        Err(Fault("unsupported_native_version" | "unsupported")) => {
            return finish(
                paths,
                &id,
                &["waiting"],
                "failed",
                "unsupported_native_version",
            );
        }
        Err(_) => return waiting(paths, &id, "provider_unavailable"),
    };
    let observed =
        match modes::session_status::observe_fresh_public(&mut control, &conversation).await {
            Ok(o) => o,
            Err(_) => return waiting(paths, &id, "provider_unavailable"),
        };
    if let Some(reason) = observed.reason {
        return waiting(paths, &id, reason);
    }
    match observed.state.as_str() {
        // No active turn. A failed or interrupted last turn leaves a live session that can
        // compact; only a running turn (or one waiting on the operator) cannot.
        "idle" | "errored" | "interrupted" => {}
        "working" => return waiting(paths, &id, "turn_active"),
        "disconnected" => return waiting(paths, &id, "session_not_running"),
        _ => return waiting(paths, &id, "state_unverified"),
    }
    // Input accepted into the native queue before the request runs first; a start now would
    // overtake it. New input is held meanwhile, so the queue drains and this check passes.
    let queued: Result<QueuePage> = control
        .request(
            "thread/queue/list",
            json!({"threadId":conversation,"cursor":null,"limit":1}),
        )
        .await;
    match queued {
        Ok(page) if page.data.is_empty() => {}
        Ok(_) => return waiting(paths, &id, "native_queue_not_empty"),
        Err(_) => return waiting(paths, &id, "provider_unavailable"),
    }
    let baseline = match recent_turns(&mut control, &conversation).await {
        Ok(turns) => turns.into_iter().map(|t| t.id).collect::<Vec<_>>(),
        Err(_) => return waiting(paths, &id, "provider_unavailable"),
    };
    if attachment(paths, &conversation)
        .map(|a| a.generation())
        .ok()
        .as_deref()
        != Some(r.runtime_generation.as_str())
    {
        return finish(paths, &id, &["waiting"], "failed", "generation_changed");
    }
    // Journal the intent before touching the app-server. Lost to a cancel or expiry: stop.
    let started = update(paths, &id, |r| {
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
            let _ = update(paths, &id, |r| {
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
            let _ = update(paths, &id, |r| {
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
    let activity = activity(&mut control, conversation)
        .await
        .map_err(|e| ("thread_read", e))?;
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
            return finish(
                paths,
                &id,
                &["starting", "running"],
                "failed",
                "generation_changed",
            );
        }
        Err(_) => None,
    };
    let since = r.started_at.unwrap_or(r.updated_at);
    if now - since > RUN_LIMIT_SECONDS {
        return finish(
            paths,
            &id,
            &["starting", "running"],
            "failed",
            "compaction_timeout",
        );
    }
    let Some(attached) = attached else {
        // No attachment: the process is gone. A starting one never started; a running one
        // cannot be followed.
        return finish(
            paths,
            &id,
            &["starting", "running"],
            "failed",
            "compaction_not_observed",
        );
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
                    let _ = update(paths, &id, |r| {
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
                }
                "failed" | "interrupted" => {
                    let reason = if turn.status == "failed" {
                        "compaction_failed"
                    } else {
                        "compaction_interrupted"
                    };
                    let _ = update(paths, &id, |r| {
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
                    let _ = update(paths, &id, |r| {
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
        None if observed.activity == "idle" && now - since > OBSERVE_GRACE_SECONDS => {
            note(&id, "idle_without_compaction_turn", "failing", now, since);
            finish(
                paths,
                &id,
                &["starting", "running"],
                "failed",
                "compaction_not_observed",
            );
        }
        None if observed.activity == "idle" || observed.compacting => progressed(&id, now),
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
    finish(
        paths,
        id,
        &["starting", "running"],
        "failed",
        "compaction_not_observed",
    );
}

/// Supervisor loop (`SESSIONSATION_COMPACTION_SECONDS`, 0 disables).
pub async fn monitor(paths: Paths) {
    let every = std::env::var("SESSIONSATION_COMPACTION_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(POLL_SECONDS);
    if every == 0 {
        return;
    }
    loop {
        // A panic in one pass must not end the loop for good.
        if let Err(panic) = std::panic::AssertUnwindSafe(tick(&paths))
            .catch_unwind()
            .await
        {
            let text = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown");
            eprintln!("compaction monitor: a pass panicked and will be retried: {text}");
        }
        tokio::time::sleep(crate::timing::secs(every)).await;
    }
}

/// `api capabilities.session_compact`.
pub fn capability() -> Value {
    json!({"since_contract":"1.47.0","command":"session compact","provider":["codex"],
        "unsupported":{"claude":"COMPACT_UNSUPPORTED","gemini":"COMPACT_UNSUPPORTED","antigravity":"COMPACT_UNSUPPORTED"},
        "when":["idle"],"native_method":"thread/compact/start","types_into_terminal":false,
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
