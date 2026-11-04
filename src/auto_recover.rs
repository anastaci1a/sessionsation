//! Per-session auto-recover (contract 1.35.0): when enabled for a live session and its
//! process later disappears without an explicit stop (host restart, crash), the
//! supervisor resumes the same conversation, preferably in the HERDR pane it last
//! ran in, and sends it a prompt.
//!
//! The setting lives in `registry/auto-recover.json` (crash-safe writes), not in the
//! catalog entry, so pane bookkeeping never changes a session generation. Every
//! recovery is journaled in `operations/auto-recoveries/<id>.json`, driven in-process
//! by the supervisor, and never replayed automatically. Phases:
//!
//! ```text
//! admitted -> pane_ready -> resuming -> resumed -> prompting -> complete
//!    \_ failed (before the resume was dispatched; nothing was started)
//!    \_ uncertain (after dispatch, or an ambiguous prompt send) -> recover | abandoned
//! ```
use crate::{
    catalog::{self, Session},
    daemon, external,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{ProviderId, claude, native},
    terminal,
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

/// Consecutive not-live checks before a recovery is attempted.
pub const MISS_THRESHOLD: u32 = 3;
/// Automatic recoveries allowed per session per hour.
pub const MAX_PER_HOUR: usize = 3;
const HOUR: i64 = 3600;
const MAX_PROMPT_BYTES: usize = 4000;
const ORDER: &[&str] = &[
    "admitted",
    "pane_ready",
    "resuming",
    "resumed",
    "prompting",
    "complete",
];
pub const PHASES_TERMINAL: &[&str] = &["complete", "failed", "abandoned"];

fn order(phase: &str) -> usize {
    ORDER.iter().position(|p| *p == phase).unwrap_or(usize::MAX)
}
fn seconds(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// -------------------------------------------------------------------- settings

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Setting {
    pub enabled: bool,
    #[serde(default)]
    pub prompt: Option<String>,
    /// HERDR target (`herdr/<session>/<pane>`) the live process last ran in.
    #[serde(default)]
    pub last_pane: Option<String>,
    /// Label of that pane's workspace, to find it again if ids changed.
    #[serde(default)]
    pub workspace_label: Option<String>,
    #[serde(default)]
    pub last_recovered_at: Option<i64>,
    #[serde(default)]
    pub last_result: Option<Value>,
    /// A deliberate restart/account switch is in progress: no recovery until live again.
    #[serde(default)]
    pub suppressed: bool,
    #[serde(default)]
    pub misses: u32,
    /// Unix seconds of recent automatic recoveries (rate limit window).
    #[serde(default)]
    pub recoveries: Vec<i64>,
    #[serde(default)]
    pub enabled_at: i64,
}
#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, Setting>,
}

fn store_file(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::AUTO_RECOVER)
}
fn load(paths: &Paths) -> Result<Store> {
    match fs::read(store_file(paths)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("UNREADABLE_AUTO_RECOVER_SETTINGS"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e.into()),
    }
}
/// Read-modify-write under an exclusive lock; the file is replaced atomically.
fn with_store<R>(paths: &Paths, f: impl FnOnce(&mut Store) -> R) -> Result<R> {
    private_dir(&paths.root.join(crate::layout::REGISTRY))?;
    let lock = lock_file(&paths.root.join(crate::layout::AUTO_RECOVER_LOCK))?;
    lock.lock_exclusive()?;
    let mut store = load(paths)?;
    let before = serde_json::to_vec(&store)?;
    let out = f(&mut store);
    store.schema_version = 1;
    let after = serde_json::to_vec(&store)?;
    if before != after {
        atomic_write(&store_file(paths), &after)?;
    }
    Ok(out)
}
fn update(paths: &Paths, id: &str, f: impl FnOnce(&mut Setting)) {
    let _ = with_store(paths, |s| {
        if let Some(setting) = s.sessions.get_mut(id) {
            f(setting);
        }
    });
}

/// Public projection (`api state` rows and `api auto-recover` receipts).
pub fn public(setting: Option<&Setting>) -> Value {
    let default = Setting::default();
    let s = setting.unwrap_or(&default);
    let now = daemon::now();
    json!({"enabled":s.enabled,"prompt":s.prompt,"last_pane":s.last_pane,"last_recovered_at":s.last_recovered_at,
        "last_result":s.last_result,"workspace_label":s.workspace_label,"suppressed":s.suppressed && s.enabled,
        "recoveries_last_hour":s.recoveries.iter().filter(|t| now - **t < HOUR).count(),
        "max_per_hour":MAX_PER_HOUR})
}
/// Settings of every session, for `api state` (an unreadable file reads as none).
pub fn all(paths: &Paths) -> BTreeMap<String, Setting> {
    load(paths).map(|s| s.sessions).unwrap_or_default()
}

/// The user stopped, archived or deleted the session: a deliberate stop is never undone.
pub fn explicit_stop(paths: &Paths, session: &str) {
    update(paths, session, |s| {
        if s.enabled {
            s.enabled = false;
            s.last_result = Some(result(
                "disabled",
                "EXPLICIT_STOP",
                "Disabled by an explicit stop, archive or delete.",
                None,
            ));
        }
    });
}
/// A deliberate restart or account switch: recovery is suppressed until the
/// session is observed live again, then resumes watching.
pub fn suppress(paths: &Paths, session: &str) {
    update(paths, session, |s| {
        if s.enabled {
            s.suppressed = true;
            s.misses = 0;
        }
    });
}

fn result(state: &str, code: &str, detail: &str, operation: Option<&str>) -> Value {
    json!({"state":state,"code":code,"detail":detail,"operation_id":operation,"at":daemon::now()})
}
fn default_prompt(at: i64) -> String {
    let when = chrono_like(at);
    format!(
        "Auto-recovery: the host restarted at {when} and sessionsation resumed this session. Re-check your state and continue your work."
    )
}
/// `YYYY-MM-DD HH:MM:SS UTC` without a date dependency.
fn chrono_like(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    let rem = unix.rem_euclid(86400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ------------------------------------------------------------------- liveness

enum Status {
    Live(Option<String>),
    Down,
    Unknown,
}
struct Seen {
    native: Option<BTreeMap<String, (claude::Observation, PathBuf)>>,
    codex: Option<Vec<external::Instance>>,
}
async fn observe_all(paths: &Paths, sessions: &[Session]) -> Seen {
    let native_needed = sessions.iter().any(|s| native::of(&s.provider).is_some());
    let codex_needed = sessions.iter().any(|s| native::of(&s.provider).is_none());
    Seen {
        native: if native_needed {
            native::reconcile(paths).ok()
        } else {
            None
        },
        codex: if codex_needed {
            crate::api::inventory(paths).await.ok()
        } else {
            None
        },
    }
}
fn herdr_pane(target: Option<&str>) -> Option<String> {
    target
        .filter(|t| t.starts_with("herdr/") && terminal::target(t).is_ok())
        .map(str::to_owned)
}
fn status_of(s: &Session, seen: &Seen) -> Status {
    if native::of(&s.provider).is_some() {
        let Some(map) = &seen.native else {
            return Status::Unknown;
        };
        return match map.get(&s.id) {
            Some((o, _)) if o.availability == "running" => Status::Live(herdr_pane(
                o.attachment
                    .as_ref()
                    .or(s.attachment.as_ref())
                    .map(|a| a.target.as_str()),
            )),
            Some((o, _)) if o.availability == "stopped" => Status::Down,
            _ => Status::Unknown,
        };
    }
    let Some(items) = &seen.codex else {
        return Status::Unknown;
    };
    let mut mine = items.iter().filter(|i| i.thread_id == s.conversation_id);
    if let Some(live) = mine.clone().find(|i| !i.inactive) {
        return Status::Live(herdr_pane(Some(&live.pane)));
    }
    if mine.any(|i| i.inactive) {
        Status::Down
    } else {
        Status::Unknown
    }
}

/// Liveness for `enable`: an interactive, non-hosted session with a running process.
async fn live_for_enable(paths: &Paths, s: &Session) -> Result<Option<String>> {
    if let Some(provider) = native::of(&s.provider) {
        if provider == ProviderId::Claude {
            ensure!(
                !claude::hosted::live(paths, &s.conversation_id),
                "AUTO_RECOVER_UNSUPPORTED: a Sessionsation-hosted (stream-json) session has no terminal to resume into"
            );
        }
        let account = native::account(paths, provider, &s.account)
            .map_err(|_| anyhow::anyhow!("ACCOUNT_UNAVAILABLE: {}:{}", s.provider, s.account))?;
        let seen = observe_all(paths, std::slice::from_ref(s)).await;
        let current = catalog::find(paths, &s.id)?;
        let o = seen
            .native
            .as_ref()
            .and_then(|m| m.get(&s.id).map(|(o, _)| o.clone()))
            .unwrap_or_else(|| {
                native::observe(
                    provider,
                    &account.home,
                    &s.conversation_id,
                    current.attachment.as_ref(),
                )
            });
        ensure!(
            o.availability == "running",
            "AUTO_RECOVER_REQUIRES_LIVE: the session is {}; auto-recover can only be enabled for a live (active or idle) session",
            o.availability
        );
        ensure!(
            o.native_kind.as_deref().is_none_or(|k| k == "interactive") && o.job_id.is_none(),
            "AUTO_RECOVER_UNSUPPORTED: background Claude sessions cannot be resumed into a pane"
        );
        return Ok(herdr_pane(
            o.attachment
                .as_ref()
                .or(current.attachment.as_ref())
                .map(|a| a.target.as_str()),
        ));
    }
    ensure!(
        s.provider == "codex" && !matches!(s.account.as_str(), "external" | "unknown"),
        "AUTO_RECOVER_UNSUPPORTED: {} sessions have no resume path",
        s.provider
    );
    let items = crate::api::inventory(paths).await?;
    let mine: Vec<_> = items
        .iter()
        .filter(|i| i.thread_id == s.conversation_id && !i.inactive)
        .collect();
    ensure!(
        mine.len() <= 1,
        "AMBIGUOUS_SESSION: more than one process holds this conversation"
    );
    let live = mine.first().filter(|i| !i.archived).with_context(|| {
        "AUTO_RECOVER_REQUIRES_LIVE: the session is not running; auto-recover can only be enabled for a live (active or idle) session"
    })?;
    Ok(herdr_pane(Some(&live.pane)))
}
/// Why `enable` would be refused for a row, for `capability_reasons` (None = allowed).
pub fn capability_reason(value: &Value, already_enabled: bool) -> Option<&'static str> {
    if already_enabled {
        return None;
    }
    let provider = value["provider"].as_str().unwrap_or_default();
    if !matches!(provider, "codex" | "claude" | "gemini" | "antigravity") {
        return Some("AUTO_RECOVER_UNSUPPORTED");
    }
    if !value["hosted"].is_null() {
        return Some("AUTO_RECOVER_UNSUPPORTED: hosted sessions have no terminal to resume into");
    }
    if value["native_kind"]
        .as_str()
        .is_some_and(|k| k != "interactive")
    {
        return Some("AUTO_RECOVER_UNSUPPORTED: background sessions cannot be resumed into a pane");
    }
    if value["availability"] != "running" {
        return Some("AUTO_RECOVER_REQUIRES_LIVE");
    }
    if provider == "codex" && matches!(value["account"].as_str(), Some("external" | "unknown")) {
        return Some("AUTO_RECOVER_UNSUPPORTED");
    }
    None
}

/// The `capabilities` descriptor (contract 1.35.0).
pub fn capability() -> Value {
    json!({"since_contract":"1.35.0","version":1,"command":"auto-recover",
        "args":["--session","--expected-generation","--enable","--disable","--prompt"],
        "providers":["codex","claude","gemini","antigravity"],
        "enable_requires":"live_session","disable":"always_allowed","session_capability":"auto_recover",
        "state_field":"auto_recover","setting_storage":"registry/auto-recover.json","changes_session_generation":false,
        "trigger":["supervisor_startup","supervisor_loop"],"consecutive_checks":MISS_THRESHOLD,"check_seconds":15,
        "max_per_hour":MAX_PER_HOUR,"operation_kind":"auto_recover","operation_dir":crate::layout::AUTO_RECOVERIES,
        "phases":["admitted","pane_ready","resuming","resumed","prompting","complete","failed","uncertain","abandoned"],
        "replays":false,"recover":"recover","abandon":"abandon-operation",
        "pane_reuse":"recorded_pane_only_if_idle_shell","fallback":["new_tab_in_workspace_by_id_and_label","new_workspace"],
        "prompt_delivery":"api_send_submit_once",
        "explicit_stop_disables":["stop","archive","delete"],"switch_or_restart":"suppressed_until_live_again",
        "refusal_codes":["AUTO_RECOVER_REQUIRES_LIVE","AUTO_RECOVER_UNSUPPORTED","STALE_TARGET","SESSION_OPERATION_IN_PROGRESS","INVALID_ARGUMENT","ACCOUNT_UNAVAILABLE","AMBIGUOUS_SESSION"],
        "last_result_states":["recovered","recovered_without_prompt","failed","uncertain","rate_limited","waiting","disabled","abandoned"]})
}

// -------------------------------------------------------------------------- API

/// `api auto-recover`: enable (live sessions only) or disable (always allowed).
pub async fn set(
    paths: &Paths,
    session: &str,
    expected_generation: &str,
    enable: bool,
    prompt: Option<String>,
) -> Result<Value> {
    let s = catalog::resolve(paths, session, expected_generation)?;
    if !enable {
        ensure!(
            prompt.is_none(),
            "INVALID_ARGUMENT: --prompt needs --enable"
        );
        let changed = with_store(paths, |store| match store.sessions.get_mut(&s.id) {
            Some(setting) if setting.enabled => {
                setting.enabled = false;
                setting.suppressed = false;
                setting.misses = 0;
                setting.last_result = Some(result(
                    "disabled",
                    "DISABLED",
                    "Disabled by the operator.",
                    None,
                ));
                true
            }
            _ => false,
        })?;
        return Ok(receipt(paths, &s, changed));
    }
    let prompt = match prompt {
        Some(p) => {
            ensure!(
                !p.trim().is_empty() && p.len() <= MAX_PROMPT_BYTES && !p.contains('\0'),
                "INVALID_ARGUMENT: --prompt must be 1-{MAX_PROMPT_BYTES} bytes of text"
            );
            Some(p)
        }
        None => None,
    };
    ensure!(
        !external::archiving::hidden(paths)?.contains(&s.id),
        "AUTO_RECOVER_REQUIRES_LIVE: the session is archived"
    );
    crate::lifecycle::ensure_available(paths, &s.conversation_id, None)?;
    let pane = live_for_enable(paths, &s).await?;
    let label = match &pane {
        Some(p) => workspace_label_of(p),
        None => None,
    };
    let changed = with_store(paths, |store| {
        let setting = store.sessions.entry(s.id.clone()).or_default();
        let changed = !setting.enabled || setting.prompt != prompt;
        setting.enabled = true;
        setting.prompt = prompt;
        setting.suppressed = false;
        setting.misses = 0;
        if changed {
            setting.enabled_at = daemon::now();
            setting.last_result = None;
        }
        if pane.is_some() {
            setting.last_pane = pane.clone();
            setting.workspace_label = label.clone().or(setting.workspace_label.take());
        }
        changed
    })?;
    Ok(receipt(paths, &s, changed))
}
fn receipt(paths: &Paths, s: &Session, changed: bool) -> Value {
    let store = all(paths);
    json!({"session_id":s.id,"generation":s.generation(),"changed":changed,"auto_recover":public(store.get(&s.id))})
}

// ---------------------------------------------------------------------- HERDR

fn herdr_session_of(target: Option<&str>) -> String {
    target
        .and_then(|t| terminal::target(t).ok())
        .map_or_else(|| "default".to_owned(), |(session, _)| session.to_owned())
}
fn workspace_of_pane(pane_id: &str) -> Option<&str> {
    pane_id.split_once(':').map(|(w, _)| w)
}
/// Best effort: the label of the workspace a recorded pane belongs to.
fn workspace_label_of(target: &str) -> Option<String> {
    let (session, pane) = terminal::target(target).ok()?;
    let workspace = workspace_of_pane(pane)?;
    let list = terminal::call(session, &["workspace", "list"]).ok()?;
    list["workspaces"]
        .as_array()?
        .iter()
        .find(|w| w["workspace_id"] == workspace)
        .and_then(|w| w["label"].as_str().map(str::to_owned))
}
/// The workspace to open a tab in: the recorded id while it still carries the
/// recorded label (ids change when a workspace closes), else the one with that label.
pub fn pick_workspace(
    list: &Value,
    recorded_id: Option<&str>,
    label: Option<&str>,
) -> Option<String> {
    let all = list["workspaces"].as_array()?;
    if let Some(id) = recorded_id
        && let Some(w) = all.iter().find(|w| w["workspace_id"] == id)
        && label.is_none_or(|l| w["label"] == l)
    {
        return Some(id.to_owned());
    }
    let label = label?;
    all.iter()
        .find(|w| w["label"] == label)
        .and_then(|w| w["workspace_id"].as_str().map(str::to_owned))
}
/// The recorded pane may be reused only while it exists and an idle shell is all
/// that runs in it (foreground group is the shell, a single shell process).
fn reusable(target: &str) -> bool {
    let Ok((session, pane)) = terminal::target(target) else {
        return false;
    };
    let Ok(info) = terminal::call(session, &["pane", "process-info", "--pane", pane]) else {
        return false;
    };
    let info = &info["process_info"];
    if let Some(group) = info["foreground_process_group_id"].as_u64()
        && info["shell_pid"].as_u64() != Some(group)
    {
        return false;
    }
    terminal::process_state(info).is_ok_and(|(idle, _)| idle)
}
fn created_pane(created: &Value) -> Result<String> {
    created["root_pane"]["pane_id"]
        .as_str()
        .map(str::to_owned)
        .context("HERDR pane ID missing")
}
/// A pane to resume into: the old one when reusable, else a new tab in the old
/// workspace (found by label if ids changed), else a new workspace.
/// Returns the HERDR target and whether it is the recorded pane.
fn choose_pane(
    hs: &str,
    recorded: Option<&str>,
    label: Option<&str>,
    cwd: &str,
    name: &str,
) -> Result<(String, bool)> {
    if let Some(old) = recorded
        && reusable(old)
    {
        label_pane(old, name);
        return Ok((old.to_owned(), true));
    }
    let list = terminal::call(hs, &["workspace", "list"])?;
    let recorded_ws = recorded
        .and_then(|t| terminal::target(t).ok())
        .and_then(|(_, p)| workspace_of_pane(p));
    let created = match pick_workspace(&list, recorded_ws, label) {
        Some(ws) => terminal::call(
            hs,
            &[
                "tab",
                "create",
                "--workspace",
                &ws,
                "--cwd",
                cwd,
                "--label",
                name,
                "--env",
                "SESSIONSATION_LAUNCHER_BYPASS=1",
                "--no-focus",
            ],
        )?,
        None => {
            let created = terminal::call(
                hs,
                &[
                    "workspace",
                    "create",
                    "--cwd",
                    cwd,
                    "--label",
                    label.unwrap_or("sessionsation"),
                    "--env",
                    "SESSIONSATION_LAUNCHER_BYPASS=1",
                    "--no-focus",
                ],
            )?;
            if let Some(tab) = created["tab"]["tab_id"].as_str() {
                let _ = terminal::call(hs, &["tab", "rename", tab, name]);
            }
            created
        }
    };
    let target = terminal::herdr_target(hs, &created_pane(&created)?)?;
    Ok((target, false))
}
/// Presentation only: name the pane and its tab after the session.
fn label_pane(target: &str, name: &str) {
    let Ok((session, pane)) = terminal::target(target) else {
        return;
    };
    let _ = terminal::call(session, &["pane", "rename", pane, name]);
    if let Ok(list) = terminal::call(session, &["pane", "list"])
        && let Some(tab) = list["panes"]
            .as_array()
            .and_then(|p| p.iter().find(|p| p["pane_id"] == pane))
            .and_then(|p| p["tab_id"].as_str())
    {
        let _ = terminal::call(session, &["tab", "rename", tab, name]);
    }
}

// -------------------------------------------------------------------- journal

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    schema_version: u32,
    id: String,
    kind: String,
    session_id: String,
    /// Top level so lifecycle admission blocks other operations on the conversation.
    conversation_id: String,
    provider: String,
    account: String,
    name: String,
    herdr_session: String,
    recorded_pane: Option<String>,
    workspace_label: Option<String>,
    directory: Option<String>,
    /// HERDR target the session is resumed into.
    pane: Option<String>,
    pane_reused: Option<bool>,
    prompt: String,
    message_id: String,
    /// pending | dispatching | delivered | failed
    prompt_state: String,
    prompt_attempts: u32,
    prompt_detail: String,
    /// pending | dispatching | confirmed | unconfirmed
    resume: String,
    phase: String,
    detail: String,
    uncertain_from: Option<String>,
    generation_after: Option<String>,
    created_at: i64,
    updated_at: i64,
}
fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::AUTO_RECOVERIES)
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
fn worker_lock_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(file(paths, id)?.with_extension("worker.lock"))
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|f| f.is_file())
}
fn read(paths: &Paths, id: &str) -> Result<Record> {
    let bytes = fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?;
    serde_json::from_slice(&bytes).context("UNREADABLE_OPERATION_RECORD")
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = daemon::now();
    private_dir(&dir(paths))?;
    atomic_write(&file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
fn phase(paths: &Paths, r: &mut Record, name: &str, detail: &str) -> Result<()> {
    r.phase = name.into();
    r.detail = detail.into();
    save(paths, r)
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
pub fn is_terminal(r: &Record) -> bool {
    PHASES_TERMINAL.contains(&r.phase.as_str())
}
fn worker_stopped(paths: &Paths, r: &Record) -> bool {
    worker_lock_path(paths, &r.id)
        .and_then(|p| crate::background::worker_stopped(&p))
        .unwrap_or(false)
}
fn stalled(paths: &Paths, r: &Record) -> bool {
    !is_terminal(r) && daemon::now() - r.updated_at > 10 && worker_stopped(paths, r)
}
fn public_record(paths: &Paths, r: &Record) -> Value {
    let stalled = stalled(paths, r);
    let can_recover =
        !is_terminal(r) && (r.phase == "uncertain" || stalled) && worker_stopped(paths, r);
    json!({"id":r.id,"session_id":r.session_id,"source_session_id":r.session_id,"kind":"auto_recover","provider":r.provider,
    "phase":r.phase,"detail":r.detail,"uncertain":r.phase == "uncertain","terminal":is_terminal(r),"stalled":stalled,
    "confirmation_seq":0,"provider_conversation_id":r.conversation_id,"account_id":format!("{}:{}",r.provider,r.account),
    "created_at":r.created_at,"updated_at":r.updated_at,"can_cancel":false,"can_recover":can_recover,"can_respond":false,
    "non_steering":true,"replays":false,
    "resume":{"state":r.resume,"pane":r.pane,"pane_reused":r.pane_reused,"recorded_pane":r.recorded_pane},
    "prompt":{"state":r.prompt_state,"attempts":r.prompt_attempts,"detail":r.prompt_detail,"message_id":r.message_id},
    "generation":r.generation_after,
    "next":match r.phase.as_str() {
        "uncertain" => "api recover <operation> to re-check from the journaled phase (a resume or prompt is never replayed), or api abandon-operation <operation> --confirm <operation>",
        "complete" | "failed" | "abandoned" => "none",
        _ if stalled => "worker stopped: api recover <operation>",
        _ => "in progress",
    }})
}
pub fn operations(paths: &Paths) -> Result<Vec<Value>> {
    Ok(list(paths)?
        .iter()
        .map(|r| public_record(paths, r))
        .collect())
}

// ------------------------------------------------------------------- recovery

/// The prompt delivery path; the supervisor uses the normal `api send`.
pub(crate) trait Sender {
    async fn send(&self, paths: &Paths, body: Value) -> Result<Value>;
}
struct Normal;
impl Sender for Normal {
    async fn send(&self, paths: &Paths, body: Value) -> Result<Value> {
        crate::messaging::headless::dispatch(paths, &serde_json::to_vec(&body)?).await
    }
}
const RETRYABLE: &[&str] = &[
    "SESSION_NOT_RUNNING",
    "STALE_TARGET",
    "LIVE_STEERING_DISABLED",
    "PEER_ENDPOINT_UNAVAILABLE",
    "PEER_SOCKET_UNLINKED",
    "SESSION_OPERATION_IN_PROGRESS",
];
/// Refusals that happen before anything is typed or journaled by the send path.
const CLEAN_REFUSAL: &[&str] = &[
    "TERMINAL_INPUT_DISABLED",
    "PEER_INTERRUPT_UNSUPPORTED",
    "INVALID_REQUEST",
    "INVALID_EXPIRY",
    "ACCOUNT_UNAVAILABLE",
    "MESSAGE_ID_CONFLICT",
];
fn code(error: &anyhow::Error) -> String {
    error
        .to_string()
        .split(|c: char| c == ':' || c.is_whitespace())
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Send the prompt once. Retried only on refusals that precede any admission;
/// anything ambiguous ends `uncertain` and is never re-sent.
async fn deliver<S: Sender>(paths: &Paths, r: &mut Record, sender: &S) -> Result<()> {
    if r.prompt_state == "delivered" {
        return Ok(());
    }
    ensure!(r.prompt_state == "pending", "PROMPT_ALREADY_DISPATCHED");
    phase(
        paths,
        r,
        "prompting",
        "Sending the recovery prompt through the normal send path (once).",
    )?;
    let deadline = std::time::Instant::now()
        + Duration::from_secs(seconds("SESSIONSATION_AUTO_RECOVER_PROMPT_SECONDS", 90));
    loop {
        let generation = catalog::find(paths, &r.session_id)?.generation();
        let body = json!({"api_version":1,"message_id":r.message_id,"session_id":r.session_id,
            "expected_generation":generation,"mode":"submit","text":r.prompt,"expires_at":daemon::now() + 600});
        r.prompt_state = "dispatching".into();
        r.prompt_attempts += 1;
        save(paths, r)?;
        match sender.send(paths, body).await {
            Ok(_) => {
                r.prompt_state = "delivered".into();
                r.prompt_detail = "Accepted by the normal send path.".into();
                return Ok(());
            }
            Err(e) => {
                let c = code(&e);
                if RETRYABLE.contains(&c.as_str()) && std::time::Instant::now() < deadline {
                    r.prompt_state = "pending".into();
                    save(paths, r)?;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                if RETRYABLE.contains(&c.as_str()) || CLEAN_REFUSAL.contains(&c.as_str()) {
                    r.prompt_state = "failed".into();
                    r.prompt_detail = format!("{e:#}");
                    return Ok(());
                }
                bail!("PROMPT_DELIVERY_UNCERTAIN: {e:#}");
            }
        }
    }
}

async fn confirmed(paths: &Paths, r: &Record) -> bool {
    let Ok(s) = catalog::find(paths, &r.session_id) else {
        return false;
    };
    let Some(pane) = &r.pane else { return false };
    if native::of(&s.provider).is_some() {
        let Ok(map) = native::reconcile(paths) else {
            return false;
        };
        return map.get(&s.id).is_some_and(|(o, _)| {
            o.availability == "running" && o.attachment.as_ref().is_some_and(|a| &a.target == pane)
        });
    }
    let items = crate::api::inventory(paths).await;
    items.is_ok_and(|items| {
        items
            .iter()
            .any(|i| !i.inactive && i.thread_id == r.conversation_id && &i.pane == pane)
    })
}

async fn settle(paths: &Paths, r: &mut Record, wait: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    loop {
        if confirmed(paths, r).await {
            r.resume = "confirmed".into();
            r.generation_after = catalog::find(paths, &r.session_id)
                .ok()
                .map(|s| s.generation());
            return phase(
                paths,
                r,
                "resumed",
                "The session is running again in its pane.",
            );
        }
        if std::time::Instant::now() >= deadline {
            r.resume = "unconfirmed".into();
            r.uncertain_from = Some("resuming".into());
            return phase(
                paths,
                r,
                "uncertain",
                "The resume was dispatched but the new process was not confirmed. It is never replayed: inspect the pane, then api recover to re-check or api abandon-operation.",
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Everything that can be refused is refused here, before a pane is touched.
async fn preflight(paths: &Paths, r: &mut Record) -> Result<Session> {
    ensure!(
        !external::archiving::hidden(paths)?.contains(&r.session_id),
        "SESSION_ARCHIVED"
    );
    let s = catalog::find(paths, &r.session_id)?;
    if let Some(_provider) = native::of(&s.provider) {
        let (s, observation) = crate::api::native_selected(paths, &s.id, &s.generation())?;
        native::ensure_resumable(&s, &observation)?;
        crate::messaging::headless::ensure_idle(paths, &s.conversation_id)?;
        r.directory = Some(native::resume_directory(&s, &observation)?);
        return Ok(s);
    }
    let items = crate::api::inventory(paths).await?;
    let s = catalog::find(paths, &r.session_id)?;
    ensure!(
        items
            .iter()
            .filter(|i| !i.inactive && i.thread_id == s.conversation_id)
            .count()
            == 0,
        "SESSION_ALREADY_RUNNING"
    );
    let source = items
        .iter()
        .find(|i| crate::api::source_matches(&s, i))
        .context("SESSION_UNAVAILABLE")?;
    ensure!(
        source.inactive && s.attachment.is_none(),
        "SESSION_ALREADY_RUNNING"
    );
    ensure!(
        matches!(source.block_reason.as_deref(), None | Some("unfinished")),
        "SESSION_STATE_UNVERIFIED"
    );
    ensure!(
        !matches!(s.account.as_str(), "external" | "unknown"),
        "ACCOUNT_UNAVAILABLE"
    );
    r.directory = Some(
        crate::api::resume_cwd(&source.cwd)?
            .to_string_lossy()
            .into_owned(),
    );
    Ok(s)
}

fn pane_command(paths: &Paths, r: &Record, generation: &str) -> Result<String> {
    use crate::terminal::shell_quote;
    let hs = &r.herdr_session;
    let mut args = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "--data-dir".into(),
        paths.root.to_string_lossy().into_owned(),
    ];
    // Same permissions and flags as a normal resume started now.
    if let Some(mode) = crate::config::launch_permissions_override() {
        args.extend(["--permissions".into(), mode.as_str().into()]);
    }
    args.extend([
        "resume".into(),
        r.session_id.clone(),
        "--expected-generation".into(),
        generation.into(),
        "--confirm-unfinished".into(),
    ]);
    Ok(format!(
        "env -u TMUX -u TMUX_PANE SESSIONSATION_HERDR_SESSION={} {}",
        shell_quote(hs),
        args.iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

/// Admission, preflight and pane choice. Nothing is started here.
async fn prepare(paths: &Paths, r: &mut Record) -> Result<()> {
    let _guard = crate::lifecycle::admit(paths, &r.conversation_id, Some(&r.id))?;
    crate::lifecycle::ensure_no_handoff(paths, &r.conversation_id, Some(&r.id))?;
    let s = preflight(paths, r).await?;
    let cwd = r.directory.clone().context("SAVED_DIRECTORY_UNAVAILABLE")?;
    // A pane chosen by an earlier attempt of this operation is tried first.
    let candidate = r.pane.clone().or(r.recorded_pane.clone());
    let (pane, reused) = choose_pane(
        &r.herdr_session,
        candidate.as_deref(),
        r.workspace_label.as_deref(),
        &cwd,
        &s.name,
    )?;
    r.pane = Some(pane);
    r.pane_reused = Some(reused && candidate == r.recorded_pane);
    phase(
        paths,
        r,
        "pane_ready",
        if reused {
            "The previous pane is an idle shell and will be reused."
        } else {
            "A new tab was opened for the resume."
        },
    )
}

async fn dispatch(paths: &Paths, r: &mut Record) -> Result<()> {
    let pane = r.pane.clone().context("SESSION_PANE_UNKNOWN")?;
    let cwd = r.directory.clone().context("SAVED_DIRECTORY_UNAVAILABLE")?;
    // The pane gets a few seconds to show its shell (a fresh tab starts one).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !terminal::state(&pane).is_ok_and(|(idle, _)| idle) {
        ensure!(
            std::time::Instant::now() < deadline,
            "PANE_NOT_AVAILABLE: the pane did not show an idle shell; the resume was not dispatched"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Final no-duplicate check on fresh evidence, then the generation the resume binds to.
    let s = catalog::find(paths, &r.session_id)?;
    let live = {
        let seen = observe_all(paths, std::slice::from_ref(&s)).await;
        matches!(status_of(&s, &seen), Status::Live(_))
    };
    ensure!(
        !live,
        "SESSION_ALREADY_RUNNING: the session is running again; nothing was started"
    );
    let s = catalog::find(paths, &r.session_id)?;
    let command = pane_command(paths, r, &s.generation())?;
    r.resume = "dispatching".into();
    phase(
        paths,
        r,
        "resuming",
        "Resume dispatched to the pane. It is never replayed.",
    )?;
    terminal::relaunch_at(&pane, &cwd, &command, None)?;
    settle(
        paths,
        r,
        seconds("SESSIONSATION_AUTO_RECOVER_CONFIRM_SECONDS", 45),
    )
    .await
}

async fn drive<S: Sender>(paths: &Paths, r: &mut Record, sender: &S) -> Result<()> {
    let p = order(&r.phase);
    if p <= order("pane_ready") {
        if p < order("pane_ready") {
            prepare(paths, r).await?;
        }
        dispatch(paths, r).await?;
    } else if r.phase == "resuming" {
        // Evidence only: a dispatched resume is never replayed.
        settle(paths, r, 5).await?;
    }
    if r.phase == "resumed" || r.phase == "prompting" {
        if r.prompt_state == "dispatching" && r.prompt_attempts > 0 {
            r.prompt_detail = "The prompt may already have been sent; it is never replayed.".into();
            return phase(
                paths,
                r,
                "complete",
                "Recovered; the prompt send was not confirmed and is not replayed.",
            );
        }
        deliver(paths, r, sender).await?;
        let detail = if r.prompt_state == "delivered" {
            "Session resumed in its pane and the recovery prompt was delivered."
        } else {
            "Session resumed in its pane; the recovery prompt was not delivered (see prompt.detail)."
        };
        phase(paths, r, "complete", detail)?;
    }
    Ok(())
}

fn fail(paths: &Paths, r: &mut Record, error: &anyhow::Error) {
    let dispatched = order(&r.phase) >= order("resuming") && r.phase != "uncertain";
    let _ = if dispatched {
        r.uncertain_from = Some(r.phase.clone());
        phase(
            paths,
            r,
            "uncertain",
            &format!(
                "{error:#}. The resume or prompt may have been dispatched; it is never replayed. Use api recover to re-check from '{}' or api abandon-operation.",
                r.phase
            ),
        )
    } else {
        phase(
            paths,
            r,
            "failed",
            &format!("{error:#}. Nothing was started."),
        )
    };
}

/// Record the outcome on the setting (`last_result`, `last_recovered_at`, `last_pane`).
fn finish(paths: &Paths, r: &Record) {
    let (state, code, detail) = match r.phase.as_str() {
        "complete" if r.prompt_state == "delivered" => ("recovered", "RECOVERED", r.detail.clone()),
        "complete" => (
            "recovered_without_prompt",
            "PROMPT_NOT_DELIVERED",
            r.detail.clone(),
        ),
        "uncertain" => ("uncertain", "RECOVERY_UNCERTAIN", r.detail.clone()),
        "failed" => ("failed", "RECOVERY_FAILED", r.detail.clone()),
        _ => return,
    };
    let confirmed = r.resume == "confirmed";
    update(paths, &r.session_id, |s| {
        s.last_result = Some(result(state, code, &detail, Some(&r.id)));
        s.misses = 0;
        if confirmed {
            s.last_recovered_at = Some(daemon::now());
            s.last_pane = r.pane.clone().or(s.last_pane.take());
        }
    });
}

async fn run<S: Sender>(paths: &Paths, r: &mut Record, sender: &S) {
    if let Err(error) = drive(paths, r, sender).await {
        fail(paths, r, &error);
    }
    finish(paths, r);
}

// ------------------------------------------------------------------------ tick

/// One pass of the supervisor loop. Cheap when no session has the setting on.
pub async fn tick(paths: &Paths) -> Result<Vec<Value>> {
    tick_with(paths, &Normal).await
}
pub(crate) async fn tick_with<S: Sender>(paths: &Paths, sender: &S) -> Result<Vec<Value>> {
    let store = load(paths)?;
    if !store.sessions.values().any(|s| s.enabled) {
        return Ok(vec![]);
    }
    private_dir(&paths.root.join(crate::layout::REGISTRY))?;
    let tick_lock = lock_file(&paths.root.join(crate::layout::AUTO_RECOVER_TICK_LOCK))?;
    if tick_lock.try_lock_exclusive().is_err() {
        return Ok(vec![]);
    }
    let mut outcomes = vec![];
    let ids: Vec<String> = store
        .sessions
        .iter()
        .filter(|(_, s)| s.enabled)
        .map(|(id, _)| id.clone())
        .collect();
    let mut sessions = vec![];
    for id in &ids {
        match catalog::find(paths, id) {
            Ok(s) => sessions.push(s),
            Err(_) => update(paths, id, |s| {
                s.enabled = false;
                s.last_result = Some(result(
                    "disabled",
                    "SESSION_GONE",
                    "The session no longer exists.",
                    None,
                ));
            }),
        }
    }
    let seen = observe_all(paths, &sessions).await;
    for s in sessions {
        let setting = load(paths)?
            .sessions
            .get(&s.id)
            .cloned()
            .unwrap_or_default();
        if !setting.enabled {
            continue;
        }
        match status_of(&s, &seen) {
            Status::Unknown => {}
            Status::Live(pane) => note_live(paths, &s, &setting, pane),
            Status::Down => {
                if setting.suppressed {
                    continue;
                }
                let misses = setting.misses + 1;
                update(paths, &s.id, |x| x.misses = misses);
                if misses >= MISS_THRESHOLD
                    && let Some(op) = attempt(paths, &s, &setting, sender).await
                {
                    outcomes.push(op);
                }
            }
        }
    }
    Ok(outcomes)
}

fn note_live(paths: &Paths, s: &Session, setting: &Setting, pane: Option<String>) {
    let moved = pane.is_some() && pane != setting.last_pane;
    if !moved && setting.misses == 0 && !setting.suppressed {
        return;
    }
    let label = if moved {
        pane.as_deref().and_then(workspace_label_of)
    } else {
        None
    };
    update(paths, &s.id, |x| {
        x.misses = 0;
        x.suppressed = false;
        if moved {
            x.last_pane = pane;
            if label.is_some() {
                x.workspace_label = label;
            }
        }
    });
}

fn note(paths: &Paths, id: &str, state: &str, code: &str, detail: &str) {
    update(paths, id, |s| {
        let same = s
            .last_result
            .as_ref()
            .is_some_and(|r| r["code"] == code && r["state"] == state);
        if !same {
            s.last_result = Some(result(state, code, detail, None));
        }
    });
}

async fn attempt<S: Sender>(
    paths: &Paths,
    s: &Session,
    setting: &Setting,
    sender: &S,
) -> Option<Value> {
    let now = daemon::now();
    if setting
        .recoveries
        .iter()
        .filter(|t| now - **t < HOUR)
        .count()
        >= MAX_PER_HOUR
    {
        note(
            paths,
            &s.id,
            "rate_limited",
            "RATE_LIMITED",
            &format!(
                "{MAX_PER_HOUR} automatic recoveries already ran within the last hour; not recovering again until that window passes. Resume it manually or disable auto-recover."
            ),
        );
        return None;
    }
    if list(paths)
        .ok()?
        .iter()
        .any(|o| o.session_id == s.id && !is_terminal(o))
    {
        note(
            paths,
            &s.id,
            "uncertain",
            "RECOVERY_UNRESOLVED",
            "An earlier recovery is unfinished or uncertain; use api recover or api abandon-operation.",
        );
        return None;
    }
    // Another operation, a hosted worker or an archive: leave it alone.
    if crate::lifecycle::pending(paths, &s.conversation_id, None)
        .ok()
        .flatten()
        .is_some()
        || claude::hosted::live(paths, &s.conversation_id)
    {
        return None;
    }
    if external::archiving::hidden(paths).ok()?.contains(&s.id) {
        explicit_stop(paths, &s.id);
        return None;
    }
    let hs = herdr_session_of(setting.last_pane.as_deref());
    if terminal::call(&hs, &["workspace", "list"]).is_err() {
        note(
            paths,
            &s.id,
            "waiting",
            "HERDR_UNAVAILABLE",
            "HERDR is not reachable; recovery will be attempted when it is.",
        );
        return None;
    }
    let prompt = setting
        .prompt
        .clone()
        .unwrap_or_else(|| default_prompt(now));
    let mut r = Record {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        kind: "auto_recover".into(),
        session_id: s.id.clone(),
        conversation_id: s.conversation_id.clone(),
        provider: s.provider.clone(),
        account: s.account.clone(),
        name: s.name.clone(),
        herdr_session: hs,
        recorded_pane: setting.last_pane.clone(),
        workspace_label: setting.workspace_label.clone(),
        directory: s.directory.clone(),
        pane: None,
        pane_reused: None,
        prompt,
        message_id: uuid::Uuid::new_v4().to_string(),
        prompt_state: "pending".into(),
        prompt_attempts: 0,
        prompt_detail: String::new(),
        resume: "pending".into(),
        phase: "admitted".into(),
        detail: "Admitted; the session process is gone.".into(),
        uncertain_from: None,
        generation_after: None,
        created_at: now,
        updated_at: now,
    };
    save(paths, &mut r).ok()?;
    let id = s.id.clone();
    let _ = with_store(paths, |store| {
        if let Some(setting) = store.sessions.get_mut(&id) {
            setting.recoveries.retain(|t| now - *t < HOUR);
            setting.recoveries.push(now);
        }
    });
    let lock = lock_file(&worker_lock_path(paths, &r.id).ok()?).ok()?;
    lock.lock_exclusive().ok()?;
    run(paths, &mut r, sender).await;
    drop(lock);
    Some(public_record(paths, &r))
}

/// The supervisor loop (`SESSIONSATION_AUTO_RECOVER_SECONDS`, default 15; 0 disables).
pub async fn monitor(paths: Paths) {
    let every = seconds("SESSIONSATION_AUTO_RECOVER_SECONDS", 15);
    if every == 0 {
        return;
    }
    // The first check runs soon after startup: a host restart is the main case.
    tokio::time::sleep(crate::timing::secs(every.min(5))).await;
    loop {
        let _ = tick(&paths).await;
        tokio::time::sleep(crate::timing::secs(every)).await;
    }
}

// -------------------------------------------------------------- recover/abandon

/// Continue from the journaled phase; a dispatched resume or prompt is only re-checked.
pub async fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let lock = lock_file(&worker_lock_path(paths, id)?)?;
    crate::paths::try_lock_brief(&lock)
        .context("SESSION_OPERATION_IN_PROGRESS: the operation's worker is still running")?;
    let mut r = read(paths, id)?;
    if is_terminal(&r) {
        return Ok(public_record(paths, &r));
    }
    if r.phase == "uncertain" {
        r.phase = r.uncertain_from.take().unwrap_or_else(|| "admitted".into());
        save(paths, &mut r)?;
    }
    run(paths, &mut r, &Normal).await;
    drop(lock);
    Ok(public_record(paths, &r))
}

/// Operator abandon of an uncertain or stuck recovery (terminal; starts nothing).
pub fn abandon(paths: &Paths, id: &str, confirm: &str) -> Result<Value> {
    ensure!(id == confirm, "CONFIRMATION_MISMATCH");
    let mut r = read(paths, id)?;
    ensure!(!is_terminal(&r), "OPERATION_ALREADY_TERMINAL");
    ensure!(
        r.phase == "uncertain" || stalled(paths, &r),
        "OPERATION_NOT_STUCK"
    );
    let from = r.phase.clone();
    phase(
        paths,
        &mut r,
        "abandoned",
        &format!(
            "Abandoned by the operator from '{from}'. Nothing was started or sent by this step."
        ),
    )?;
    update(paths, &r.session_id.clone(), |s| {
        s.last_result = Some(result(
            "abandoned",
            "ABANDONED",
            "The recovery was abandoned by the operator.",
            Some(id),
        ));
    });
    Ok(public_record(paths, &r))
}

#[cfg(test)]
mod tests;
