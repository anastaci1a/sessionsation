//! `api session set-model` (contract 1.49.0): change the model and reasoning effort of a running
//! Codex or Claude session at its next idle point, and report only what the session itself
//! shows afterwards.
//!
//! One journaled operation per session (`operations/model-changes/<id>.json`):
//! `waiting → applying → complete | failed | cancelled | expired`. The daemon supervisor
//! (`monitor`, next to the compaction one) advances it:
//!
//! - Codex: `thread/settings/update` (model, then effort) on the account app-server under the
//!   same session ownership every send takes, once the thread is idle with an empty native
//!   queue, verified with `thread/read`. Nothing is typed into a terminal. New public `submit`
//!   / `queue` input is held meanwhile (`pending_local`, reason `model_change_hold`) and
//!   released in arrival order afterwards, through the same release path as compaction holds.
//! - Claude: a detached worker (`model-change-worker`, [`claude`]) types `/model <id>` and
//!   `/effort <level>` into the session's verified HERDR pane while it is idle with an empty
//!   prompt, answers the `Switch model?` confirmation, and confirms each value only from the
//!   status-line hook record ([`crate::session_model`]); the footer text is never parsed.
//!
//! Effort is always applied after the model (a `/model` switch can change it); without
//! `--effort` the effort in force before the switch is re-applied.
use super::*;
use crate::provider::ProviderId;
use crate::session_model::{self, Observed};
use anyhow::{Context, ensure};
use futures_util::FutureExt;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub mod claude;

/// Lifetime of a change requested without `--when-idle`.
pub const NOW_EXPIRY_SECONDS: i64 = 60;
pub const DEFAULT_EXPIRY_SECONDS: i64 = 3600;
pub const MAX_EXPIRY_SECONDS: i64 = 86400;
/// `--wait` bounds (seconds).
pub const DEFAULT_WAIT_NOW: u64 = 90;
pub const MAX_WAIT_SECONDS: u64 = 600;
const MAX_ATTEMPTS: u32 = 3;
const RELEASE_CLAIM_SECONDS: i64 = 120;
pub const POLL_SECONDS: u64 = 2;
/// Retry schedule of a waiting change: first interval, doubling up to the maximum.
const CLAUDE_FIRST_BACKOFF_SECONDS: i64 = 4;
const CODEX_FIRST_BACKOFF_SECONDS: i64 = 2;
const MAX_BACKOFF_SECONDS: i64 = 30;
/// A Codex change in `applying` with no recorded progress this long fails
/// (`apply_not_observed`) and releases its hold (same bound as compaction 1.47.1).
pub const APPLY_UNOBSERVED_SECONDS: i64 = 180;

const TERMINAL: &[&str] = &["complete", "failed", "cancelled", "expired"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub schema_version: u32,
    pub id: String,
    pub session_id: String,
    pub accepted_generation: String,
    pub conversation_id: String,
    /// `codex` | `claude`
    pub provider: String,
    pub account: String,
    /// `now` | `idle`
    pub when: String,
    /// Codex: native runtime generation. Claude: `<pid>:<process start>:<pane target>`.
    pub runtime: String,
    pub requested_model: String,
    pub requested_effort: Option<String>,
    /// `requested` | `preserved` | `model_default`
    pub effort_source: String,
    pub before: Option<Observed>,
    pub observed: Option<Observed>,
    pub applied: Option<Observed>,
    /// waiting | applying | complete | failed | cancelled | expired
    pub phase: String,
    pub reason: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub steps: Vec<String>,
    pub attempts: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub hold_released: bool,
    #[serde(default)]
    pub release_claimed_at: Option<i64>,
    /// Claude: the screen around the input box when a step failed (no draft text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_layout: Option<Value>,
}
impl Record {
    pub fn terminal(&self) -> bool {
        TERMINAL.contains(&self.phase.as_str())
    }
    fn holds(&self, now: i64) -> bool {
        self.provider == "codex"
            && match self.phase.as_str() {
                "waiting" => now < self.expires_at,
                "applying" => true,
                _ => !self.hold_released,
            }
    }
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::MODEL_CHANGES)
}
fn file(paths: &Paths, id: &str) -> anyhow::Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
pub(crate) fn worker_lock(paths: &Paths, id: &str) -> anyhow::Result<PathBuf> {
    Ok(file(paths, id)?.with_extension("worker.lock"))
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|f| f.is_file())
}
pub(crate) fn read(paths: &Paths, id: &str) -> anyhow::Result<Record> {
    let bytes = fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?;
    serde_json::from_slice(&bytes).context("UNREADABLE_OPERATION_RECORD")
}
fn save(paths: &Paths, r: &Record) -> anyhow::Result<()> {
    private_dir(&dir(paths))?;
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
fn lock(paths: &Paths) -> anyhow::Result<fs::File> {
    private_dir(&paths.root.join(crate::layout::OPERATIONS))?;
    let f = lock_file(&paths.root.join(crate::layout::MODEL_CHANGES_LOCK))?;
    f.lock_exclusive()?;
    Ok(f)
}
/// Apply `edit` when it returns true (short read-modify-write; never held across an await).
pub(crate) fn update(
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
pub(crate) fn finish(paths: &Paths, id: &str, from: &[&str], phase: &str, reason: &str) {
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
pub(crate) fn waiting(paths: &Paths, id: &str, why: &str) {
    let _ = update(paths, id, |r| {
        if r.phase != "waiting" || r.reason.as_deref() == Some(why) {
            return false;
        }
        r.reason = Some(why.into());
        true
    });
}
/// End an applying operation with the observed values.
pub(crate) fn fail(paths: &Paths, id: &str, reason: &str, observed: Option<Observed>) {
    let _ = update(paths, id, |r| {
        if r.terminal() {
            return false;
        }
        r.phase = "failed".into();
        r.reason = Some(reason.into());
        if observed.is_some() {
            r.observed = observed;
        }
        r.finished_at = Some(crate::daemon::now());
        true
    });
}
/// `complete` with `applied` = the positive observation of both requested values.
pub(crate) fn complete(paths: &Paths, id: &str, reason: &str, applied: Observed) {
    let done = update(paths, id, |r| {
        if r.terminal() {
            return false;
        }
        r.phase = "complete".into();
        r.reason = Some(reason.into());
        r.observed = Some(applied.clone());
        r.applied = Some(Observed {
            source: "set_model".into(),
            ..applied.clone()
        });
        r.finished_at = Some(crate::daemon::now());
        true
    });
    if let Ok(Some(r)) = done {
        let _ = session_model::store(
            paths,
            &r.conversation_id,
            Observed {
                source: "set_model".into(),
                ..applied
            },
        );
    }
}
pub(crate) fn step(paths: &Paths, id: &str, name: &str) {
    let _ = update(paths, id, |r| {
        r.step = Some(name.into());
        if r.steps.len() < 64 {
            r.steps.push(name.into());
        }
        true
    });
}

/// Whether `observed` shows the requested model and (when one is targeted) the effort.
pub(crate) fn matches(r: &Record, target_effort: Option<&str>, o: &Observed) -> bool {
    session_model::same_model(&o.model, &r.requested_model)
        && target_effort.is_none_or(|e| {
            o.effort
                .as_deref()
                .is_some_and(|x| session_model::same(x, e))
        })
}
/// The effort to apply: requested, else the one in force before the change.
pub(crate) fn target_effort(r: &Record) -> Option<String> {
    r.requested_effort
        .clone()
        .or_else(|| r.before.as_ref().and_then(|b| b.effort.clone()))
}

/// True while new public Codex input for this conversation must be held: a model change is
/// waiting (before its expiry) or applying, or ended without its held input released yet.
pub fn hold_active(paths: &Paths, conversation: &str) -> bool {
    let now = crate::daemon::now();
    list(paths).is_ok_and(|all| {
        all.iter()
            .any(|r| r.conversation_id == conversation && r.holds(now))
    })
}
/// A non-terminal model change of the conversation requested before `(created_at, id)`.
pub(crate) fn pending_before(paths: &Paths, conversation: &str, key: (i64, &str)) -> bool {
    list(paths).is_ok_and(|all| {
        all.iter().any(|r| {
            !r.terminal()
                && r.conversation_id == conversation
                && (r.created_at, r.id.as_str()) < key
        })
    })
}
/// Any non-terminal model change of the conversation.
pub(crate) fn pending(paths: &Paths, conversation: &str) -> bool {
    list(paths).is_ok_and(|all| {
        all.iter()
            .any(|r| !r.terminal() && r.conversation_id == conversation)
    })
}

fn explain(reason: &str) -> &'static str {
    match reason {
        "turn_active" => "A turn is active; the change is applied when it ends.",
        "native_queue_not_empty" => {
            "Input already in the native queue runs first, on the old model."
        }
        "approval_or_user_input_required" => "The session waits for an approval or an answer.",
        "session_not_running" => "The session is not running with a verified attachment.",
        "session_busy" => "Another operation or delivery holds the session; retried.",
        "native_sending_disabled" => "Native sending is disabled by the owner switch.",
        "explicit_resume_required" => "The session was stopped and needs an explicit Resume.",
        "provider_unavailable" => "The account app-server could not be reached; retried.",
        "state_unverified" => "The session state could not be verified; retried.",
        "compaction_pending" => "A compaction requested earlier runs first.",
        "draft_present" => {
            "The Claude prompt holds a draft; the change waits until it is empty (Sessionsation never clears a draft for a model change)."
        }
        "terminal_input_disabled" => {
            "Terminal input is turned off (config terminal_input_enabled)."
        }
        "already_applied" => {
            "The session already runs the requested model and effort; nothing was sent."
        }
        "completed" => "The session reports the requested model and effort.",
        "expired" => "The expiry passed before the session was idle; nothing was changed.",
        "cancelled" => "Cancelled before anything was applied.",
        "generation_changed" => "The session restarted after the request; nothing was changed.",
        "provider_rejected" => "The app-server refused the settings update.",
        "unsupported_native_version" => "The account app-server version is not supported.",
        "model_not_available" => "The account's model list does not offer this model.",
        "model_not_confirmed" => {
            "The session does not report the requested model (refused, substituted or not applied)."
        }
        "effort_not_confirmed" => {
            "The session runs the requested model but not the requested effort."
        }
        "effort_unsupported" => "The model does not report an effort level.",
        "blocked_by_hook" => {
            "A PreModelSwitch hook asked for confirmation; the switch was declined."
        }
        "worker_interrupted" => {
            "The Claude worker stopped mid-procedure and the pane was not in a known state."
        }

        "apply_not_observed" => {
            "No progress could be observed for 3 minutes while applying; held input was released."
        }
        "prompt_not_owned" => "The prompt held text the worker did not type; nothing was cleared.",
        "pane_left_in_dialog" => {
            "A confirmation dialog could not be closed; the pane needs attention (see detail)."
        }
        "terminal_delivery_active" => {
            "A message is being typed into or was just delivered to this pane; retried."
        }
        _ => "",
    }
}
pub(crate) fn public(r: &Record) -> Value {
    let target = target_effort(r);
    json!({"id":r.id,"kind":"model_change","provider":r.provider,"when":r.when,
        "session_id":r.session_id,"source_session_id":r.session_id,"accepted_generation":r.accepted_generation,
        "provider_conversation_id":r.conversation_id,"phase":r.phase,"step":r.step,"terminal":r.terminal(),
        "reason":r.reason,"detail":r.detail.clone().or_else(|| r.reason.as_deref().map(explain).filter(|s| !s.is_empty()).map(str::to_owned)),
        "requested":{"model":r.requested_model,"effort":r.requested_effort},"target_effort":target,"effort_source":r.effort_source,
        "before":r.before.as_ref().map(Observed::public),"observed":r.observed.as_ref().map(Observed::public),
        "applied":r.applied.as_ref().map(Observed::public),
        "changed":r.before.as_ref().map(|b| json!({"model":!session_model::same_model(&b.model,&r.requested_model),"effort":b.effort != target})),
        "hold":{"active":r.holds(crate::daemon::now()),"released":r.hold_released,"scope":if r.provider == "codex" {"new public submit and queue sends to this session"} else {"none (Claude terminal deliveries wait for the pane lock instead)"}},
        "steps":r.steps,"refusal_layout":r.refusal_layout,
        "attempts":r.attempts,"created_at":r.created_at,"updated_at":r.updated_at,"expires_at":r.expires_at,
        "started_at":r.started_at,"finished_at":r.finished_at,
        "can_cancel":r.phase == "waiting","can_respond":false,"can_recover":false,
        "non_steering":true,"replays":false})
}

pub struct Request<'a> {
    pub session: &'a str,
    pub expected_generation: &'a str,
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub when_idle: bool,
    pub expires_at: Option<i64>,
    pub operation: Option<&'a str>,
}

/// Admission. Nothing is journaled on a refusal; one pending change per session.
pub fn request(paths: &Paths, req: Request) -> anyhow::Result<Value> {
    let session = crate::catalog::resolve(paths, req.session, req.expected_generation)?;
    let provider = match session.provider.as_str() {
        "codex" => ProviderId::Codex,
        "claude" => ProviderId::Claude,
        other => anyhow::bail!(
            "UNSUPPORTED_PROVIDER_OPERATION: {other} sessions cannot change model through Sessionsation"
        ),
    };
    if provider == ProviderId::Claude {
        crate::provider::claude::hosted::ensure_not_hosted(paths, &session.conversation_id)
            .map_err(|_| anyhow::anyhow!("UNSUPPORTED_PROVIDER_OPERATION: hosted sessions cannot change model through Sessionsation yet"))?;
    }
    session_model::validate(provider, req.model, req.effort)?;
    if let Some(id) = req.operation {
        uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    }
    let now = crate::daemon::now();
    let expires_at = if req.when_idle {
        req.expires_at.unwrap_or(now + DEFAULT_EXPIRY_SECONDS)
    } else {
        ensure!(
            req.expires_at.is_none(),
            "INVALID_ARGUMENT: --expires-at needs --when-idle"
        );
        now + NOW_EXPIRY_SECONDS
    };
    ensure!(
        expires_at > now && expires_at <= now + MAX_EXPIRY_SECONDS,
        "INVALID_EXPIRY: expires_at must be within the next {MAX_EXPIRY_SECONDS} seconds"
    );
    let (runtime, hook) = match provider {
        ProviderId::Codex => {
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
            (attached.generation(), None)
        }
        _ => {
            let (t, hook) = claude::admit(paths, &session)?;
            (claude::runtime_key(&t), Some(hook))
        }
    };
    crate::lifecycle::ensure_available(paths, &session.conversation_id, None)?;
    let _lock = lock(paths)?;
    let all = list(paths)?;
    let same_values =
        |r: &Record| r.requested_model == req.model && r.requested_effort.as_deref() == req.effort;
    if let Some(id) = req.operation
        && let Some(r) = all.iter().find(|r| r.id == id)
    {
        ensure!(
            r.conversation_id == session.conversation_id && same_values(r),
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
        ensure!(
            same_values(r),
            "SESSION_OPERATION_IN_PROGRESS: operation {} (model_change, phase {}); cancel it first",
            r.id,
            r.phase
        );
        let mut v = public(r);
        v["existing"] = json!(true);
        return Ok(v);
    }
    let before = hook.as_ref().and_then(Observed::from_hook);
    let mut r = Record {
        schema_version: 1,
        id: req
            .operation
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned),
        session_id: session.id.clone(),
        accepted_generation: req.expected_generation.to_owned(),
        conversation_id: session.conversation_id.clone(),
        provider: session.provider.clone(),
        account: session.account.clone(),
        when: if req.when_idle { "idle" } else { "now" }.into(),
        runtime,
        requested_model: req.model.to_owned(),
        requested_effort: req.effort.map(str::to_owned),
        effort_source: if req.effort.is_some() {
            "requested"
        } else {
            "preserved"
        }
        .into(),
        before: before.clone(),
        observed: before.clone(),
        applied: None,
        phase: "waiting".into(),
        reason: None,
        detail: None,
        step: None,
        steps: vec![],
        attempts: 0,
        created_at: now,
        updated_at: now,
        expires_at,
        started_at: None,
        finished_at: None,
        // Claude input is never held (terminal deliveries serialize on the pane lock).
        hold_released: provider == ProviderId::Claude,
        release_claimed_at: None,
        refusal_layout: None,
    };
    if let Some(b) = &before {
        if r.requested_effort.is_none() && b.effort.is_none() {
            r.effort_source = "model_default".into();
        }
        if matches(&r, target_effort(&r).as_deref(), b) {
            r.phase = "complete".into();
            r.reason = Some("already_applied".into());
            r.applied = Some(b.clone());
            r.finished_at = Some(now);
        }
    }
    save(paths, &r)?;
    let mut v = public(&r);
    v["existing"] = json!(false);
    Ok(v)
}

/// Wait (bounded) for the record to end. A non-`complete` end is `MODEL_NOT_CONFIRMED`.
pub async fn wait(paths: &Paths, id: &str, seconds: u64) -> anyhow::Result<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        let r = read(paths, id)?;
        if r.terminal() || std::time::Instant::now() >= deadline {
            let v = status_value(paths, &r).await;
            if r.phase == "complete" {
                return Ok(v);
            }
            return Err(session_model::NotConfirmed {
                code: "MODEL_NOT_CONFIRMED",
                message: format!(
                    "operation {} is {}: {}",
                    r.id,
                    r.phase,
                    r.reason.as_deref().unwrap_or("not finished within --wait")
                ),
                operation: v,
            }
            .into());
        }
        tokio::time::sleep(crate::timing::scaled(std::time::Duration::from_millis(250))).await;
    }
}

async fn status_value(paths: &Paths, r: &Record) -> Value {
    let mut v = public(r);
    v["held_messages"] = if r.provider == "codex" {
        match super::compaction::held_ids(paths, &r.conversation_id).await {
            Some(ids) => json!(ids),
            None => Value::Null,
        }
    } else {
        json!([])
    };
    v
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
    Ok(status_value(paths, &r).await)
}

/// Cancel a change that has not started applying. Idempotent for a cancelled one.
pub async fn cancel(paths: &Paths, id: &str) -> anyhow::Result<Value> {
    let r = read(paths, id)?;
    match r.phase.as_str() {
        "cancelled" => return Ok(public(&r)),
        "waiting" => {}
        "applying" => anyhow::bail!("OPERATION_WORKER_ACTIVE: the change is being applied"),
        _ => anyhow::bail!("OPERATION_ALREADY_TERMINAL"),
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
        let now = read(paths, id)?;
        ensure!(
            now.phase == "cancelled",
            "OPERATION_WORKER_ACTIVE: the change is being applied"
        );
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

/// Release Codex input held by a finished change, once no other idle-point operation of the
/// conversation remains (the shared compaction release path checks that).
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
    let done = super::compaction::deliver_held(paths, &r.conversation_id).await;
    let _ = update(paths, id, |r| {
        r.hold_released |= done;
        r.release_claimed_at = None;
        true
    });
}

/// Per-operation retry schedule of a waiting change: `(next attempt, current interval)`.
/// Process memory only; a supervisor restart starts over at the first interval.
static BACKOFF: Mutex<BTreeMap<String, (i64, i64)>> = Mutex::new(BTreeMap::new());

/// Whether a waiting operation is due for another attempt.
pub(crate) fn due(id: &str, now: i64) -> bool {
    BACKOFF
        .lock()
        .map(|b| b.get(id).is_none_or(|(next, _)| now >= *next))
        .unwrap_or(true)
}
/// Schedule the next attempt: `first` seconds, doubling up to `MAX_BACKOFF_SECONDS`.
pub(crate) fn back_off(id: &str, now: i64, first: i64) {
    if let Ok(mut b) = BACKOFF.lock() {
        let interval = b
            .get(id)
            .map_or(first, |(_, i)| (*i * 2).min(MAX_BACKOFF_SECONDS));
        b.insert(id.to_owned(), (now + interval, interval));
    }
}

pub async fn tick(paths: &Paths) {
    let Ok(all) = list(paths) else { return };
    let now = crate::daemon::now();
    for r in all.iter().filter(|r| !r.terminal()) {
        if r.phase == "waiting" && now >= r.expires_at {
            finish(paths, &r.id, &["waiting"], "expired", "expired");
            continue;
        }
        if r.phase == "waiting" && !due(&r.id, now) {
            continue;
        }
        if r.provider == "codex" {
            codex_advance(paths, r.clone()).await;
            if read(paths, &r.id).is_ok_and(|x| x.phase == "waiting") {
                back_off(&r.id, now, CODEX_FIRST_BACKOFF_SECONDS);
            }
        } else {
            spawn_claude_worker(paths, r, now);
        }
    }
    let Ok(all) = list(paths) else { return };
    for r in all.iter().filter(|r| r.terminal() && !r.hold_released) {
        release(paths, &r.id).await;
    }
    if let Ok(mut b) = BACKOFF.lock() {
        b.retain(|id, _| all.iter().any(|r| &r.id == id && !r.terminal()));
    }
}

/// Start the Claude worker for a waiting change (throttled), or a new one for an applying
/// change whose worker died (its lock is free); the worker decides everything itself.
fn spawn_claude_worker(paths: &Paths, r: &Record, now: i64) {
    let Ok(lock_path) = worker_lock(paths, &r.id) else {
        return;
    };
    if !crate::background::worker_stopped(&lock_path).unwrap_or(false) {
        return;
    }
    // Idle polling backs off (4 s doubling to 30 s) while the session stays busy.
    back_off(&r.id, now, CLAUDE_FIRST_BACKOFF_SECONDS);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = crate::background::detach(&mut std::process::Command::new(exe))
        .arg("--data-dir")
        .arg(&paths.root)
        .args(["model-change-worker", &r.id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[derive(Deserialize)]
struct ThreadSettingsRead {
    thread: ThreadSettings,
}
#[derive(Deserialize)]
struct ThreadSettings {
    id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "reasoningEffort")]
    reasoning_effort: Option<String>,
}
async fn read_settings(control: &mut Transport, conversation: &str) -> Result<Option<Observed>> {
    let t: ThreadSettingsRead = control
        .request(
            "thread/read",
            json!({"threadId":conversation,"includeTurns":false}),
        )
        .await?;
    if t.thread.id != conversation {
        return Err(Fault("malformed_response"));
    }
    Ok(t.thread.model.map(|model| Observed {
        model,
        effort: t.thread.reasoning_effort,
        observed_at: crate::daemon::now(),
        source: "codex_thread".into(),
    }))
}

/// Codex model and effort of a running session, read fresh from its app-server (`thread/read`).
/// Used by the one-pass launch to verify before the brief is sent. Also kept in the sidecar.
pub async fn codex_settings(paths: &Paths, conversation: &str) -> anyhow::Result<Option<Observed>> {
    let attached = attachment(paths, conversation).map_err(|e| anyhow::anyhow!("{}", e.0))?;
    let mut control = attached
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.0))?;
    let observed = read_settings(&mut control, conversation)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.0))?;
    if let Some(o) = &observed {
        let _ = session_model::store(paths, conversation, o.clone());
    }
    Ok(observed)
}

/// Whether the account's `model/list` offers `model` and (when given) `effort` for it.
/// `Ok(None)` when the list cannot be read (the verification decides then).
async fn offered(
    control: &mut Transport,
    model: &str,
    effort: Option<&str>,
) -> Option<std::result::Result<(), &'static str>> {
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let page = control.model_list_page(cursor.as_deref()).await.ok()?;
        for m in page["data"].as_array()? {
            if m["id"] == model || m["model"] == model {
                let efforts: Vec<&str> = m["supportedReasoningEfforts"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e["reasoningEffort"].as_str().or_else(|| e.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();
                return Some(match effort {
                    Some(e) if !efforts.is_empty() && !efforts.contains(&e) => {
                        Err("effort_unsupported")
                    }
                    _ => Ok(()),
                });
            }
        }
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Some(Err("model_not_available"))
}

async fn codex_advance(paths: &Paths, r: Record) {
    let id = r.id.clone();
    let conversation = r.conversation_id.clone();
    if r.phase == "applying" && crate::daemon::now() - r.updated_at > APPLY_UNOBSERVED_SECONDS {
        // No progress could be observed: never hold input for ever. The tick releases it.
        eprintln!(
            "model change {id}: no progress for {APPLY_UNOBSERVED_SECONDS} s while applying; failed apply_not_observed, held input released"
        );
        return fail(paths, &id, "apply_not_observed", None);
    }
    if r.phase == "waiting"
        && super::compaction::pending_before(paths, &conversation, (r.created_at, &r.id))
    {
        return waiting(paths, &id, "compaction_pending");
    }
    let attached = match attachment(paths, &conversation) {
        Ok(a) => a,
        Err(Fault("unavailable")) if r.phase == "waiting" => {
            return waiting(paths, &id, "session_not_running");
        }
        Err(e) if r.phase == "waiting" => return waiting(paths, &id, e.0),
        Err(_) => return fail(paths, &id, "generation_changed", None),
    };
    if attached.generation() != r.runtime {
        return finish(
            paths,
            &id,
            &["waiting", "applying"],
            "failed",
            "generation_changed",
        );
    }
    if r.phase == "waiting" {
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
    }
    let budget = std::time::Duration::from_secs(3);
    let _guard = match mutation_lock_waiting(paths, &conversation, budget).await {
        Ok(g) => g,
        Err(_) if r.phase == "waiting" => return waiting(paths, &id, "session_busy"),
        Err(_) => {
            eprintln!(
                "model change {id}: applying, session ownership busy; retried (bounded by {APPLY_UNOBSERVED_SECONDS} s)"
            );
            return;
        }
    };
    let mut control = match attached.connect().await {
        Ok(c) => c,
        Err(Fault("unsupported_native_version" | "unsupported")) => {
            return finish(
                paths,
                &id,
                &["waiting", "applying"],
                "failed",
                "unsupported_native_version",
            );
        }
        Err(_) if r.phase == "waiting" => return waiting(paths, &id, "provider_unavailable"),
        Err(e) => {
            eprintln!(
                "model change {id}: applying, app-server unreachable ({}); retried (bounded by {APPLY_UNOBSERVED_SECONDS} s)",
                e.0
            );
            return;
        }
    };
    if r.phase == "applying" {
        // A supervisor stopped mid-change: never resend blindly; the thread decides.
        return codex_verify(paths, &r, &mut control, 1).await;
    }
    let observed =
        match modes::session_status::observe_fresh_public(&mut control, &conversation).await {
            Ok(o) => o,
            Err(_) => return waiting(paths, &id, "provider_unavailable"),
        };
    if let Some(reason) = observed.reason {
        return waiting(paths, &id, reason);
    }
    match observed.state.as_str() {
        "idle" | "errored" | "interrupted" => {}
        "working" => return waiting(paths, &id, "turn_active"),
        "disconnected" => return waiting(paths, &id, "session_not_running"),
        _ => return waiting(paths, &id, "state_unverified"),
    }
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
    let before = match read_settings(&mut control, &conversation).await {
        Ok(b) => b,
        Err(_) => return waiting(paths, &id, "provider_unavailable"),
    };
    if let Some(Err(reason)) = offered(
        &mut control,
        &r.requested_model,
        r.requested_effort.as_deref(),
    )
    .await
    {
        return finish(paths, &id, &["waiting"], "failed", reason);
    }
    let mut r = r;
    r.before = before.clone();
    let effort = target_effort(&r);
    if let Some(b) = &before
        && matches(&r, effort.as_deref(), b)
    {
        let _ = update(paths, &id, |x| {
            x.before = before.clone();
            true
        });
        return complete(paths, &id, "already_applied", b.clone());
    }
    let started = update(paths, &id, |x| {
        if x.phase != "waiting" || crate::daemon::now() >= x.expires_at {
            return false;
        }
        x.phase = "applying".into();
        x.reason = None;
        x.attempts += 1;
        x.before = before.clone();
        x.observed = before.clone();
        if x.requested_effort.is_none() && effort.is_none() {
            x.effort_source = "model_default".into();
        }
        x.started_at = Some(crate::daemon::now());
        true
    });
    let Ok(Some(r)) = started else { return };
    let model_due = before
        .as_ref()
        .is_none_or(|b| !session_model::same_model(&b.model, &r.requested_model));
    if model_due {
        step(paths, &id, "model");
        if let Err(e) = control
            .settings_update(&conversation, Some(&r.requested_model), None)
            .await
        {
            if e.0 == "native_rejected" {
                return fail(paths, &id, "provider_rejected", before);
            }
            // Unknown outcome: the read decides.
            return codex_verify(paths, &r, &mut control, 5).await;
        }
    }
    if let Some(e) = &effort {
        step(paths, &id, "effort");
        if let Err(err) = control.settings_update(&conversation, None, Some(e)).await
            && err.0 == "native_rejected"
        {
            let now = read_settings(&mut control, &conversation)
                .await
                .ok()
                .flatten();
            return fail(paths, &id, "provider_rejected", now);
        }
    }
    codex_verify(paths, &r, &mut control, 5).await
}

/// Read the thread until it shows the requested values (at most `reads`, 1 s apart).
async fn codex_verify(paths: &Paths, r: &Record, control: &mut Transport, reads: u32) {
    step(paths, &r.id, "verify");
    let effort = target_effort(r);
    let mut last = None;
    let mut read_any = false;
    for i in 0..reads.max(1) {
        if i > 0 {
            tokio::time::sleep(crate::timing::secs(1)).await;
        }
        match read_settings(control, &r.conversation_id).await {
            Ok(Some(o)) if matches(r, effort.as_deref(), &o) => {
                return complete(paths, &r.id, "completed", o);
            }
            Ok(o) => {
                read_any = true;
                last = o;
            }
            Err(_) => {}
        }
    }
    if !read_any {
        // Nothing could be read: no verdict; the next pass reads again (bounded by
        // APPLY_UNOBSERVED_SECONDS).
        eprintln!(
            "model change {}: applying, thread/read failed; retried",
            r.id
        );
        return;
    }
    let unchanged = last.as_ref().zip(r.before.as_ref()).is_some_and(|(l, b)| {
        session_model::same_model(&l.model, &b.model) && l.effort == b.effort
    });
    if unchanged && r.attempts < MAX_ATTEMPTS && reads > 1 {
        // Positively not applied: wait for the next idle point and try again.
        let _ = update(paths, &r.id, |x| {
            if x.phase != "applying" {
                return false;
            }
            x.phase = "waiting".into();
            x.reason = Some("model_not_confirmed".into());
            true
        });
        return;
    }
    let reason = match &last {
        Some(l) if session_model::same_model(&l.model, &r.requested_model) => {
            "effort_not_confirmed"
        }
        _ => "model_not_confirmed",
    };
    fail(paths, &r.id, reason, last);
}

/// Supervisor loop (`SESSIONSATION_MODEL_SECONDS`, 0 disables).
pub async fn monitor(paths: Paths) {
    let every = std::env::var("SESSIONSATION_MODEL_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(POLL_SECONDS);
    if every == 0 {
        return;
    }
    loop {
        for pass in [
            tick(&paths).boxed(),
            crate::session_launch::tick(&paths).boxed(),
        ] {
            if let Err(panic) = std::panic::AssertUnwindSafe(pass).catch_unwind().await {
                let text = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown");
                eprintln!("model monitor: a pass panicked and will be retried: {text}");
            }
        }
        tokio::time::sleep(crate::timing::secs(every)).await;
    }
}

/// `api capabilities.session_model`.
pub fn capability() -> Value {
    json!({"since_contract":"1.49.0","version":1,
        "row_field":"model","row_shape":["id","effort","observed_at","source"],
        "sources":["claude_statusline","codex_thread","set_model"],"exact_ids_only":true,"effort_after_model":true,
        "claude_model_grammar":"claude-<lower-case letters, digits, dots, dashes>[1m]?","claude_efforts":session_model::CLAUDE_EFFORTS,
        "codex_efforts":crate::launch::REASONING_EFFORTS,
        "set":{"command":"session set-model","status":"session set-model-status","cancel":"session set-model-cancel",
            "cancel_operation":"cancel-operation","model_required":true,"when":["now","idle"],"pending_per_session":1,
            "now_expiry_seconds":NOW_EXPIRY_SECONDS,"default_expiry_seconds":DEFAULT_EXPIRY_SECONDS,"max_expiry_seconds":MAX_EXPIRY_SECONDS,
            "wait":{"flag":"--wait","default_now":DEFAULT_WAIT_NOW,"default_idle":0,"max":MAX_WAIT_SECONDS},
            "phases":["waiting","applying","complete","failed","cancelled","expired"],
            "providers":{"codex":{"native_methods":["thread/settings/update","thread/read","model/list"],"types_into_terminal":false},
                "claude":{"commands":["/model <id>","/effort <level>"],"types_into_terminal":true,
                    "requires":["herdr_terminal","terminal_input_enabled","statusline_record","empty_prompt"],"verified_from":"statusline_hook_payload",
                    "dialogs":["Switch model?","Change effort level?"],"never":["ctrl+c","ctrl+d","digit keys"],"esc":"only to close its own confirmation dialog when its No option cannot be chosen","dialog_left_open":"never (pane_left_in_dialog names the pane)","idle_checks":{"reads":2,"gap_ms":1500,"delivery_quiet_seconds":5},"backoff_seconds":{"claude":[4,30],"codex":[2,30]},"applying_unobserved_seconds":APPLY_UNOBSERVED_SECONDS,"resume_passes_recorded_model":true,
                    "side_effect":"Claude Code saves the model and effort as the account default for later starts without --model"}},
            "unsupported":{"gemini":"UNSUPPORTED_PROVIDER_OPERATION","antigravity":"UNSUPPORTED_PROVIDER_OPERATION","claude_hosted":"UNSUPPORTED_PROVIDER_OPERATION"},
            "hold":{"codex":{"scope":"new public submit and queue sends","held_receipt":{"state":"pending_local","reason":"model_change_hold"},"released":"in arrival order when no compaction or model change of the session remains"},
                "claude":"not held; terminal deliveries wait for the pane lock the worker holds while applying"},
            "ordering":"compactions and model changes of one session run in request order",
            "not_confirmed_error":"MODEL_NOT_CONFIRMED","journal":crate::layout::MODEL_CHANGES,"replays":false},
        "launch":crate::session_launch::capability()})
}

#[cfg(test)]
mod tests;
