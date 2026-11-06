//! Per-session Auto Start (contract 1.44.0; "auto-recover" since 1.35.0). The mode is
//! independent of whether the session runs. A mode other than `disabled` starts the same
//! conversation again, preferably in the HERDR pane it last ran in, on exactly two triggers:
//!
//! * host boot: the supervisor sees a `boot_id` different from the persisted one and starts
//!   every non-disabled session that is not running (also ones stopped explicitly before the
//!   reboot), once per boot;
//! * crash: on the same boot, a session that was seen running and whose process then ended
//!   abnormally (see [`classify`]). A clean exit, a closed pane, an explicit stop or an exit
//!   that cannot be classified starts nothing.
//!
//! Mode `start` sends nothing; mode `start_prompt` then sends a prompt (custom or default).
//!
//! The setting lives in `registry/auto-recover.json` (crash-safe writes), not in the
//! catalog entry, so pane bookkeeping never changes a session generation. Every
//! recovery is journaled in `operations/auto-recoveries/<id>.json`, driven in-process
//! by the supervisor, and never replayed automatically. Phases:
//!
//! ```text
//! admitted -> pane_ready -> resuming -> resumed -> prompting -> complete   (start_prompt)
//! admitted -> pane_ready -> resuming -> resumed -> complete                (start: no prompting)
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

/// What happens when a watched session's process disappears.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Disabled,
    /// Resume only; nothing is sent.
    Start,
    /// Resume, then send the prompt (custom or default) once.
    StartPrompt,
}
impl Mode {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "disabled" => Ok(Self::Disabled),
            "start" => Ok(Self::Start),
            "start_prompt" => Ok(Self::StartPrompt),
            _ => bail!("INVALID_ARGUMENT: --mode must be disabled, start or start_prompt"),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Start => "start",
            Self::StartPrompt => "start_prompt",
        }
    }
}
fn default_record_mode() -> String {
    "start_prompt".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Setting {
    /// Mirrors `mode != disabled` (kept so a downgrade still reads the old field).
    pub enabled: bool,
    /// Absent in records written before 1.43.0: those read as `start_prompt` when enabled
    /// (see [`Setting::mode`]) and are rewritten by [`migrate`].
    #[serde(default)]
    pub mode: Option<Mode>,
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
    /// Host boot (`boot_id`) this session is due a boot start for; cleared once started,
    /// seen running, or disabled.
    #[serde(default)]
    pub boot_due: Option<String>,
    /// `boot_id` of the boot this session was last started for (at most one per boot).
    #[serde(default)]
    pub boot_started: Option<String>,
    /// Array start (`array_id`) this session is due a start for; cleared once started,
    /// seen running, or disabled.
    #[serde(default)]
    pub array_due: Option<String>,
    /// `array_id` of the array start this session was last started for.
    #[serde(default)]
    pub array_started: Option<String>,
    /// Unix seconds of the last explicit `api stop`; cleared when the session runs again.
    /// It never changes the mode.
    #[serde(default)]
    pub stopped_explicitly_at: Option<i64>,
    /// Process last seen running (pid, /proc start ticks) and the boot it ran in: the
    /// subject of crash classification.
    #[serde(default)]
    pub live_pid: Option<u32>,
    #[serde(default)]
    pub live_start: Option<u64>,
    #[serde(default)]
    pub live_boot: Option<String>,
}
impl Setting {
    pub fn mode(&self) -> Mode {
        self.mode.unwrap_or(if self.enabled {
            Mode::StartPrompt
        } else {
            Mode::Disabled
        })
    }
    fn set_mode(&mut self, mode: Mode) {
        self.mode = Some(mode);
        self.enabled = mode != Mode::Disabled;
        if mode == Mode::Disabled {
            self.boot_due = None;
            self.array_due = None;
        }
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, Setting>,
    /// `boot_id` the supervisor last saw (absent before 1.44.0: the first pass records it
    /// and starts nothing).
    #[serde(default)]
    boot_id: Option<String>,
    /// Mount id of `/mnt/user` the supervisor last saw (the array start epoch, see
    /// `array_id`; absent before 1.45.0: the first pass records it and starts nothing).
    #[serde(default)]
    array_id: Option<String>,
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
    // Compared with the file as stored, so records from before 1.43.0 are rewritten
    // (with their mode filled in) by the first write.
    let before = fs::read(store_file(paths)).unwrap_or(serde_json::to_vec(&store)?);
    for setting in store.sessions.values_mut() {
        setting.mode = Some(setting.mode());
    }
    let out = f(&mut store);
    store.schema_version = 2;
    let after = serde_json::to_vec(&store)?;
    if before != after {
        atomic_write(&store_file(paths), &after)?;
    }
    Ok(out)
}
/// Storage migration (1.43.0): every record gets an explicit `mode` (an enabled record
/// from before becomes `start_prompt`, prompt kept). One atomic replace under the store
/// lock, so a crash leaves the old or the new file; running it again changes nothing.
pub fn migrate(paths: &Paths) -> Result<()> {
    if store_file(paths).is_file() {
        with_store(paths, |_| ())?;
    }
    Ok(())
}
fn update(paths: &Paths, id: &str, f: impl FnOnce(&mut Setting)) {
    let _ = with_store(paths, |s| {
        if let Some(setting) = s.sessions.get_mut(id) {
            f(setting);
        }
    });
}

/// Public projection (`api state` rows and `api auto-recover` receipts).
/// Deprecated `auto_recover` projection (since 1.43.0; kept for older clients).
pub fn public(setting: Option<&Setting>) -> Value {
    let default = Setting::default();
    let s = setting.unwrap_or(&default);
    let now = daemon::now();
    json!({"enabled":s.mode() != Mode::Disabled,"prompt":s.prompt,"last_pane":s.last_pane,"last_recovered_at":s.last_recovered_at,
        "last_result":legacy_result(s.last_result.as_ref()),"workspace_label":s.workspace_label,"suppressed":s.suppressed && s.enabled,
        "recoveries_last_hour":s.recoveries.iter().filter(|t| now - **t < HOUR).count(),
        "max_per_hour":MAX_PER_HOUR})
}
/// `auto_start` projection (contract 1.43.0): the old fields plus `mode`, with
/// `last_started_at` in place of `last_recovered_at`.
pub fn public_start(setting: Option<&Setting>) -> Value {
    let default = Setting::default();
    let s = setting.unwrap_or(&default);
    let now = daemon::now();
    json!({"mode":s.mode().as_str(),"enabled":s.mode() != Mode::Disabled,"prompt":s.prompt,"last_pane":s.last_pane,
        "last_started_at":s.last_recovered_at,"last_result":s.last_result,"workspace_label":s.workspace_label,
        "suppressed":s.suppressed && s.enabled,
        "starts_last_hour":s.recoveries.iter().filter(|t| now - **t < HOUR).count(),
        "max_per_hour":MAX_PER_HOUR})
}
/// The 1.35.0 names of the new result states, for the deprecated projection.
fn legacy_result(result: Option<&Value>) -> Value {
    let Some(r) = result else { return Value::Null };
    let mut r = r.clone();
    match r["state"].as_str() {
        Some(state) if state.starts_with("started") && state.ends_with("_with_prompt") => {
            r["state"] = json!("recovered");
            r["code"] = json!("RECOVERED");
        }
        Some(state) if state.starts_with("started") => {
            r["state"] = json!("recovered_without_prompt");
            r["code"] = json!("PROMPT_NOT_DELIVERED");
        }
        // Nothing was started and nothing failed: an old client sees a pending wait.
        Some("skipped_clean_exit" | "exit_unclassified" | "skipped_explicit_stop") => {
            r["state"] = json!("waiting");
        }
        _ => {}
    }
    r
}
/// Settings of every session, for `api state` (an unreadable file reads as none).
pub fn all(paths: &Paths) -> BTreeMap<String, Setting> {
    load(paths).map(|s| s.sessions).unwrap_or_default()
}

/// `api stop` (1.44.0): only marks that the session was stopped intentionally. The mode
/// is kept, no crash start follows, and the next host boot starts it again.
pub fn explicit_stop(paths: &Paths, session: &str) {
    update(paths, session, |s| {
        s.stopped_explicitly_at = Some(daemon::now());
        s.live_pid = None;
        s.live_start = None;
        s.live_boot = None;
        s.misses = 0;
        if s.enabled {
            s.last_result = Some(result(
                "skipped_explicit_stop",
                "EXPLICIT_STOP",
                "Stopped explicitly: no crash start; the mode is kept and applies at the next host boot.",
                None,
            ));
        }
    });
}
/// Archive or delete: the setting is switched off for good.
pub fn retire(paths: &Paths, session: &str) {
    update(paths, session, |s| {
        if s.enabled {
            s.set_mode(Mode::Disabled);
            s.last_result = Some(result(
                "disabled",
                "ARCHIVED_OR_DELETED",
                "Disabled because the session was archived or deleted.",
                None,
            ));
        }
        s.live_pid = None;
        s.live_start = None;
        s.live_boot = None;
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
/// The default prompt: after a host restart (`crash` None, `at` = boot time) or after a
/// crash (`crash` = what was seen, `at` = now).
fn default_prompt(at: i64, crash: Option<&str>) -> String {
    let when = chrono_like(at);
    match crash {
        None => format!(
            "Auto-start: the host restarted at {when} and sessionsation started this session again after the restart. Re-check your state and continue your work."
        ),
        Some(what) => format!(
            "Auto-start: this session's process crashed ({what}) and sessionsation started it again at {when}. Re-check your state and continue your work."
        ),
    }
}
fn array_prompt(at: i64) -> String {
    format!(
        "Auto-start: the array was restarted at {} and sessionsation started this session again after the restart. Re-check your state and continue your work.",
        chrono_like(at)
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

/// Process identity of a running session: pid and /proc start ticks.
type Proc = Option<(u32, u64)>;
enum Status {
    Live(Option<String>, Proc),
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
            Some((o, _)) if o.availability == "running" => Status::Live(
                herdr_pane(
                    o.attachment
                        .as_ref()
                        .or(s.attachment.as_ref())
                        .map(|a| a.target.as_str()),
                ),
                o.pid.zip(o.process_start),
            ),
            Some((o, _)) if o.availability == "stopped" => Status::Down,
            _ => Status::Unknown,
        };
    }
    let Some(items) = &seen.codex else {
        return Status::Unknown;
    };
    let mut mine = items.iter().filter(|i| i.thread_id == s.conversation_id);
    if let Some(live) = mine.clone().find(|i| !i.inactive) {
        return Status::Live(
            herdr_pane(Some(&live.pane)),
            Some((live.pid, live.start_time)),
        );
    }
    if mine.any(|i| i.inactive) {
        Status::Down
    } else {
        Status::Unknown
    }
}

/// Checks for `enable` (an interactive, non-hosted session; running or not) and the pane
/// its running process is in, if any.
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
            o.native_kind.as_deref().is_none_or(|k| k == "interactive") && o.job_id.is_none(),
            "AUTO_RECOVER_UNSUPPORTED: background Claude sessions cannot be resumed into a pane"
        );
        // A stopped or saved session may have the setting too (1.44.0): no pane to record.
        if o.availability != "running" {
            return Ok(None);
        }
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
    Ok(mine
        .first()
        .filter(|i| !i.archived)
        .and_then(|live| herdr_pane(Some(&live.pane))))
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
    // Running state is irrelevant since 1.44.0; only archived sessions are excluded.
    if value["archived"] == true {
        return Some("AUTO_RECOVER_UNSUPPORTED: archived sessions are not started automatically");
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
        "enable_requires":"none","disable":"always_allowed","session_capability":"auto_recover",
        "state_field":"auto_recover","setting_storage":"registry/auto-recover.json","changes_session_generation":false,
        "trigger":["supervisor_startup","supervisor_loop"],"consecutive_checks":MISS_THRESHOLD,"check_seconds":15,
        "max_per_hour":MAX_PER_HOUR,"operation_kind":"auto_recover","operation_dir":crate::layout::AUTO_RECOVERIES,
        "phases":["admitted","pane_ready","resuming","resumed","prompting","complete","failed","uncertain","abandoned"],
        "replays":false,"recover":"recover","abandon":"abandon-operation",
        "pane_reuse":"recorded_pane_only_if_idle_shell","fallback":["new_tab_in_workspace_by_id_and_label","new_workspace"],
        "prompt_delivery":"api_send_submit_once",
        "explicit_stop_disables":["archive","delete"],"explicit_stop":"marks_only","switch_or_restart":"suppressed_until_live_again",
        "refusal_codes":["AUTO_RECOVER_UNSUPPORTED","STALE_TARGET","SESSION_OPERATION_IN_PROGRESS","INVALID_ARGUMENT","ACCOUNT_UNAVAILABLE","AMBIGUOUS_SESSION"],
        "last_result_states":["recovered","recovered_without_prompt","failed","uncertain","rate_limited","waiting","disabled","abandoned"],
        "note":"from 1.44.0 skipped and unclassified exits read as waiting",
        "deprecated":true,"replaced_by":"auto_start"})
}

/// The `capabilities.auto_start` descriptor (contract 1.43.0).
pub fn capability_start() -> Value {
    json!({"since_contract":"1.43.0","version":1,"command":"auto-start",
        "args":["--session","--expected-generation","--mode","--prompt","--all"],
        "modes":["disabled","start","start_prompt"],"default_mode":"disabled",
        "default_prompt":"Auto-start: the host restarted at <UTC time> and sessionsation started this session again after the restart. Re-check your state and continue your work.",
        "default_prompt_crash":"Auto-start: this session's process crashed (<what was seen>) and sessionsation started it again at <UTC time>. Re-check your state and continue your work.",
        "revised_in":"1.44.0","explicit_stop":"marks_only","explicit_stop_disables":["archive","delete"],
        "triggers":{
            "host_boot":{"detect":"/proc/sys/kernel/random/boot_id differs from the persisted boot id",
                "scope":"every session with a mode that is not running, including explicitly stopped ones",
                "once_per_boot":true,"daemon_restart_starts_nothing":true},
            "array_start":{"detect":"the kernel mount id of /mnt/user (the shfs array mount) differs from the persisted one on the same boot",
                "scope":"every session with a mode that is not running",
                "once_per_array_start":true,"daemon_restart_starts_nothing":true},
            "crash":{"scope":"same boot; the session was seen running and its process ended abnormally",
                "crash_evidence":["kernel_oom_kill","kernel_segfault","kernel_fatal_trap"],
                "not_crashes":["clean_exit","closed_pane_sighup_sigterm","explicit_stop"],
                "unknown_exit":"no start; last_result exit_unclassified",
                "classifiable":{
                    "all_providers":"kernel log (dmesg) lines naming the recorded pid: OOM kill (SIGKILL), segfault (SIGSEGV), fatal trap (SIGILL, SIGSEGV, SIGFPE)",
                    "claude":"registry entry sessions/<pid>.json removed = orderly shutdown (skipped_clean_exit); left behind without kernel evidence = exit_unclassified",
                    "codex_gemini_antigravity":"kernel log only; any other exit is exit_unclassified"},
                "unclassifiable":["SIGABRT and runtime aborts without a kernel log line","manual kill -9 without a kernel log line","a non-zero exit that still shuts down cleanly","the HERDR pane exit status (HERDR does not expose one)"]}},
        "max_prompt_bytes":MAX_PROMPT_BYTES,"clear_prompt":"api auto-start --mode start_prompt without --prompt resets to the default prompt",
        "mode_semantics":{"start":"resume only, nothing is sent","start_prompt":"resume, then send the prompt (custom or default) once"},
        "prompt":"only_with_start_prompt","bulk":{"flag":"--all","mode":"disabled","operator_only":true},
        "enable_requires":"none","disable":"always_allowed","session_capability":"auto_start",
        "state_field":"auto_start","setting_storage":"registry/auto-recover.json","changes_session_generation":false,
        "deprecated_aliases":{"command":"auto-recover","state_field":"auto_recover","capability":"auto_recover"},
        "trigger":["host_boot","crash"],"consecutive_checks":MISS_THRESHOLD,"check_seconds":15,
        "max_per_hour":MAX_PER_HOUR,"operation_kind":"auto_recover","operation_dir":crate::layout::AUTO_RECOVERIES,
        "phases":["admitted","pane_ready","resuming","resumed","prompting","complete","failed","uncertain","abandoned"],
        "replays":false,"recover":"recover","abandon":"abandon-operation",
        "last_result_states":["started_after_array_start","started_after_array_start_with_prompt","started_after_boot","started_after_boot_with_prompt","started_after_crash","started_after_crash_with_prompt","skipped_clean_exit","exit_unclassified","skipped_explicit_stop","failed","uncertain","rate_limited","waiting","disabled","abandoned"],
        "refusal_codes":["AUTO_RECOVER_UNSUPPORTED","STALE_TARGET","SESSION_OPERATION_IN_PROGRESS","INVALID_ARGUMENT","ACCOUNT_UNAVAILABLE","AMBIGUOUS_SESSION"]})
}

// -------------------------------------------------------------------------- API

/// Deprecated `api auto-recover`: `--enable` is `start_prompt`, `--disable` is `disabled`.
pub async fn set(
    paths: &Paths,
    session: &str,
    expected_generation: &str,
    enable: bool,
    prompt: Option<String>,
) -> Result<Value> {
    if !enable {
        ensure!(
            prompt.is_none(),
            "INVALID_ARGUMENT: --prompt needs --enable"
        );
    }
    let mode = if enable {
        Mode::StartPrompt
    } else {
        Mode::Disabled
    };
    set_mode(paths, session, expected_generation, mode, prompt).await
}

/// `api auto-start --mode`: set the mode, whether or not the session runs (1.44.0).
pub async fn set_mode(
    paths: &Paths,
    session: &str,
    expected_generation: &str,
    mode: Mode,
    prompt: Option<String>,
) -> Result<Value> {
    let s = catalog::resolve(paths, session, expected_generation)?;
    ensure!(
        prompt.is_none() || mode == Mode::StartPrompt,
        "INVALID_ARGUMENT: --prompt is only valid with --mode start_prompt"
    );
    if mode == Mode::Disabled {
        let changed = with_store(paths, |store| match store.sessions.get_mut(&s.id) {
            Some(setting) if setting.mode() != Mode::Disabled => {
                setting.set_mode(Mode::Disabled);
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
    if let Some(p) = &prompt {
        ensure!(
            !p.trim().is_empty() && p.len() <= MAX_PROMPT_BYTES && !p.contains('\0'),
            "INVALID_ARGUMENT: --prompt must be 1-{MAX_PROMPT_BYTES} bytes of text"
        );
    }
    let already = all(paths)
        .get(&s.id)
        .is_some_and(|x| x.mode() != Mode::Disabled);
    let mut pane = None;
    let mut label = None;
    if !already {
        ensure!(
            !external::archiving::hidden(paths)?.contains(&s.id),
            "AUTO_RECOVER_UNSUPPORTED: the session is archived"
        );
        crate::lifecycle::ensure_available(paths, &s.conversation_id, None)?;
        pane = live_for_enable(paths, &s).await?;
        label = pane.as_deref().and_then(workspace_label_of);
    }
    let changed = with_store(paths, |store| {
        let setting = store.sessions.entry(s.id.clone()).or_default();
        let changed = setting.mode() != mode || setting.prompt != prompt;
        setting.set_mode(mode);
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
/// `api auto-start --all --mode disabled`: disable every session (operator bulk admin).
pub fn disable_all(paths: &Paths) -> Result<Value> {
    let ids = with_store(paths, |store| {
        let mut ids = vec![];
        for (id, setting) in store.sessions.iter_mut() {
            if setting.mode() != Mode::Disabled {
                setting.set_mode(Mode::Disabled);
                setting.suppressed = false;
                setting.misses = 0;
                setting.last_result = Some(result(
                    "disabled",
                    "DISABLED",
                    "Disabled by the operator (all sessions).",
                    None,
                ));
                ids.push(id.clone());
            }
        }
        ids
    })?;
    Ok(
        json!({"all":true,"mode":"disabled","changed":!ids.is_empty(),"disabled_count":ids.len(),"disabled":ids}),
    )
}
fn receipt(paths: &Paths, s: &Session, changed: bool) -> Value {
    let store = all(paths);
    json!({"session_id":s.id,"generation":s.generation(),"changed":changed,"mode":store.get(&s.id).map_or("disabled",|x| x.mode().as_str()),
        "auto_start":public_start(store.get(&s.id)),"auto_recover":public(store.get(&s.id))})
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
    /// boot | crash (empty on operations from before 1.44.0).
    #[serde(default)]
    trigger: String,
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
    /// start | start_prompt (records from before 1.43.0 prompted).
    #[serde(default = "default_record_mode")]
    mode: String,
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
    "mode":r.mode,"prompt":{"state":r.prompt_state,"attempts":r.prompt_attempts,"detail":r.prompt_detail,"message_id":r.message_id},
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
    /// The current receipt of a sent message.
    async fn receipt(&self, paths: &Paths, id: &str) -> Result<Value> {
        crate::messaging::receipt_public(paths, id, false).await
    }
    /// Claude only: the live process has published its native peer endpoint
    /// (`<home>/sessions/<pid>.json` and its socket).
    async fn native_ready(&self, paths: &Paths, session_id: &str) -> bool {
        native_endpoint_ready(paths, session_id)
    }
    fn poll(&self) -> Duration {
        Duration::from_millis(500)
    }
    fn backoff(&self, attempt: u32) -> Duration {
        Duration::from_secs((2u64 << attempt.min(3)).min(15))
    }
}
struct Normal;
impl Sender for Normal {
    async fn send(&self, paths: &Paths, body: Value) -> Result<Value> {
        crate::messaging::headless::dispatch(paths, &serde_json::to_vec(&body)?).await
    }
}
fn native_endpoint_ready(paths: &Paths, session_id: &str) -> bool {
    let Ok(s) = catalog::find(paths, session_id) else {
        return false;
    };
    let Ok(account) = native::account(paths, ProviderId::Claude, &s.account) else {
        return false;
    };
    let Ok(live) = claude::live_sessions(&account.home) else {
        return false;
    };
    claude::holder(&live, &s.conversation_id).is_some_and(|l| {
        claude::native_peer::accepting(&account.home, l.pid)
            && claude::peer::endpoint(&account.home, l).is_ok()
    })
}
const RETRYABLE: &[&str] = &[
    "SESSION_NOT_RUNNING",
    "STALE_TARGET",
    "LIVE_STEERING_DISABLED",
    "PEER_ENDPOINT_UNAVAILABLE",
    "PEER_SOCKET_UNLINKED",
    "SESSION_OPERATION_IN_PROGRESS",
    "ADAPTER_BUSY",
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

/// What a message receipt says about the prompt.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// written, input consumed, or accepted natively (or later).
    Delivered(String),
    /// Refused and provably nothing written: safe to retry with a fresh id.
    Refused(String),
    /// Not final yet.
    Pending,
    /// Anything that may have been written but is not confirmed.
    Uncertain(String),
}
fn outcome(v: &Value) -> Outcome {
    let v = v.get("result").unwrap_or(v);
    let state = v["state"].as_str().unwrap_or("unknown");
    let reason = v["reason"].as_str().unwrap_or("");
    let label = if reason.is_empty() {
        state.to_owned()
    } else {
        format!("{state}/{reason}")
    };
    let facts = &v["facts"];
    let written = facts["written"].as_bool().unwrap_or(false);
    if written
        || facts["input_consumed"] == true
        || matches!(
            state,
            "native_accepted"
                | "execution_started"
                | "completed_with_reply"
                | "completed_without_reply"
        )
    {
        return Outcome::Delivered(label);
    }
    let steps = v["terminal_input"]["steps"]
        .as_array()
        .is_none_or(|s| s.is_empty());
    if state == "rejected" && !written && steps && facts["input_recorded"] != true {
        return Outcome::Refused(label);
    }
    if matches!(state, "pending_local" | "held") {
        return Outcome::Pending;
    }
    Outcome::Uncertain(label)
}

fn add_detail(r: &mut Record, text: String) {
    if !r.prompt_detail.is_empty() {
        r.prompt_detail.push_str("; ");
    }
    r.prompt_detail.push_str(&text);
}

/// Deliver the prompt and read the receipt to a final state. `delivered` means
/// written / consumed / natively accepted. A rejected receipt (nothing written)
/// and a pre-admission refusal are retried with backoff (a fresh message id for
/// a rejected receipt) while the session keeps its generation; anything that
/// may have been written ends `uncertain` and is never re-sent.
async fn deliver<S: Sender>(paths: &Paths, r: &mut Record, sender: &S) -> Result<()> {
    if r.prompt_state == "delivered" {
        return Ok(());
    }
    ensure!(r.prompt_state == "pending", "PROMPT_ALREADY_DISPATCHED");
    phase(
        paths,
        r,
        "prompting",
        "Sending the recovery prompt through the normal send path.",
    )?;
    let started = std::time::Instant::now();
    let deadline =
        started + Duration::from_secs(seconds("SESSIONSATION_AUTO_RECOVER_PROMPT_SECONDS", 180));
    let first_generation = catalog::find(paths, &r.session_id)?.generation();
    let claude = r.provider == "claude";
    // Prefer a native peer message: wait (bounded) for the registry entry and socket.
    let mut native = false;
    if claude {
        let wait = std::time::Instant::now()
            + Duration::from_secs(seconds("SESSIONSATION_AUTO_RECOVER_NATIVE_SECONDS", 60));
        loop {
            if sender.native_ready(paths, &r.session_id).await {
                native = true;
                break;
            }
            if std::time::Instant::now() >= wait {
                break;
            }
            tokio::time::sleep(sender.poll()).await;
        }
    }
    let mut refused = 0u32;
    loop {
        let generation = catalog::find(paths, &r.session_id)?.generation();
        if generation != first_generation {
            r.prompt_state = "failed".into();
            add_detail(r, "the session changed generation; not retried".into());
            return Ok(());
        }
        let mut body = json!({"api_version":1,"message_id":r.message_id,"session_id":r.session_id,
            "expected_generation":generation,"mode":"submit","text":r.prompt,"expires_at":daemon::now() + 600});
        if native {
            body["delivery"] = json!("native");
        }
        r.prompt_state = "dispatching".into();
        r.prompt_attempts += 1;
        save(paths, r)?;
        let attempt = r.prompt_attempts;
        let id = r.message_id.clone();
        let sent = match sender.send(paths, body).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let c = code(&e);
                if RETRYABLE.contains(&c.as_str()) {
                    Err(format!("{c} (before admission)"))
                } else if CLEAN_REFUSAL.contains(&c.as_str()) {
                    r.prompt_state = "failed".into();
                    add_detail(r, format!("attempt {attempt}: {e:#}"));
                    return Ok(());
                } else {
                    add_detail(r, format!("attempt {attempt} ({id}): {e:#}"));
                    bail!("PROMPT_DELIVERY_UNCERTAIN: {e:#}");
                }
            }
        };
        let (label, fresh_id) = match sent {
            Err(label) => (label, false),
            Ok(mut v) => {
                // Read the receipt to a final state.
                let settle_by = std::time::Instant::now() + Duration::from_secs(30);
                let out = loop {
                    match outcome(&v) {
                        Outcome::Pending if std::time::Instant::now() < settle_by => {
                            tokio::time::sleep(sender.poll()).await;
                            match sender.receipt(paths, &id).await {
                                Ok(next) => v = next,
                                Err(e) => {
                                    break Outcome::Uncertain(format!("receipt unreadable: {e:#}"));
                                }
                            }
                        }
                        Outcome::Pending => {
                            break Outcome::Uncertain("not final after 30 s".into());
                        }
                        other => break other,
                    }
                };
                match out {
                    Outcome::Delivered(label) => {
                        r.prompt_state = "delivered".into();
                        add_detail(r, format!("attempt {attempt} ({id}): {label}"));
                        return Ok(());
                    }
                    Outcome::Uncertain(label) => {
                        add_detail(r, format!("attempt {attempt} ({id}): {label}"));
                        bail!("PROMPT_DELIVERY_UNCERTAIN: receipt {label}");
                    }
                    Outcome::Refused(label) => (label, true),
                    Outcome::Pending => unreachable!(),
                }
            }
        };
        add_detail(r, format!("attempt {attempt} ({id}): {label}"));
        refused += 1;
        if std::time::Instant::now() >= deadline {
            r.prompt_state = "failed".into();
            add_detail(r, format!("not delivered after {refused} refused attempts"));
            return Ok(());
        }
        if fresh_id {
            r.message_id = uuid::Uuid::new_v4().to_string();
        }
        r.prompt_state = "pending".into();
        save(paths, r)?;
        tokio::time::sleep(sender.backoff(refused)).await;
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
        matches!(status_of(&s, &seen), Status::Live(..))
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
        if r.mode == "start" {
            r.prompt_state = "skipped".into();
            r.prompt_detail = "Mode start: no prompt is sent.".into();
            return phase(
                paths,
                r,
                "complete",
                "Session resumed in its pane; mode start sends no prompt.",
            );
        }
        deliver(paths, r, sender).await?;
        let detail = if r.prompt_state == "delivered" {
            "Session resumed in its pane and the auto-start prompt was delivered."
        } else {
            "Session resumed in its pane; the auto-start prompt was not delivered (see prompt.detail)."
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
/// A start names its trigger (`started_after_boot`, `started_after_crash`, each with an
/// optional `_with_prompt`); operations from before 1.44.0 have none and keep the old names.
fn finish(paths: &Paths, r: &Record) {
    let started = |with_prompt: bool, code: &str| {
        let base = match r.trigger.as_str() {
            "boot" => "started_after_boot",
            "array_start" => "started_after_array_start",
            "crash" => "started_after_crash",
            _ => "started",
        };
        let state = if with_prompt {
            format!("{base}_with_prompt")
        } else {
            base.to_owned()
        };
        let code = if code.is_empty() {
            state.to_uppercase()
        } else {
            code.to_owned()
        };
        (state, code, r.detail.clone())
    };
    let (state, code, detail) = match r.phase.as_str() {
        "complete" if r.prompt_state == "delivered" => started(true, ""),
        "complete" if r.mode == "start" => started(
            false,
            &if r.trigger.is_empty() {
                "STARTED".to_owned()
            } else {
                String::new()
            },
        ),
        "complete" => started(false, "PROMPT_NOT_DELIVERED"),
        "uncertain" => (
            "uncertain".into(),
            "RECOVERY_UNCERTAIN".into(),
            r.detail.clone(),
        ),
        "failed" => ("failed".into(), "RECOVERY_FAILED".into(), r.detail.clone()),
        _ => return,
    };
    let confirmed = r.resume == "confirmed";
    update(paths, &r.session_id, |s| {
        s.last_result = Some(result(&state, &code, &detail, Some(&r.id)));
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

// ------------------------------------------------------------- boot and crash

/// The current host boot (`SESSIONSATION_BOOT_ID_FILE` overrides the path, for tests).
fn boot_id() -> Option<String> {
    let path = std::env::var("SESSIONSATION_BOOT_ID_FILE")
        .unwrap_or_else(|_| "/proc/sys/kernel/random/boot_id".into());
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
/// The array start epoch: the kernel mount id of `/mnt/user` (the shfs union mount that only
/// exists while the array is started). Every mount gets a new id, so a stop/start of the
/// array on the same boot changes it, while a supervisor or daemon restart never does. It
/// needs no hook or token file. `SESSIONSATION_MOUNTINFO_FILE` replaces the mountinfo file
/// and `SESSIONSATION_ARRAY_MOUNT` the mount point (tests).
fn array_id() -> Option<String> {
    let file = std::env::var("SESSIONSATION_MOUNTINFO_FILE")
        .unwrap_or_else(|_| "/proc/self/mountinfo".into());
    let point = std::env::var("SESSIONSATION_ARRAY_MOUNT").unwrap_or_else(|_| "/mnt/user".into());
    array_id_in(&fs::read_to_string(file).ok()?, &point)
}
fn array_id_in(mountinfo: &str, point: &str) -> Option<String> {
    mountinfo.lines().rev().find_map(|l| {
        let mut f = l.split_whitespace();
        let id = f.next()?;
        (f.nth(3)? == point).then(|| id.to_owned())
    })
}
/// When the host booted (Unix seconds), for the boot prompt; falls back to `now`.
fn boot_time() -> i64 {
    fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
        })
        .unwrap_or_else(daemon::now)
}
/// The kernel log (`dmesg`; `SESSIONSATION_KERNEL_LOG_FILE` replaces it, for tests).
fn kernel_log() -> Option<String> {
    if let Ok(path) = std::env::var("SESSIONSATION_KERNEL_LOG_FILE") {
        return fs::read_to_string(path).ok();
    }
    let out = std::process::Command::new("dmesg").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Compare the boot with the persisted one. A different boot makes every session with a
/// mode that has not been started for it due a boot start; the same boot (a daemon or
/// supervisor restart) changes nothing; the very first pass only records the boot.
fn detect_boot(paths: &Paths, current: &str) -> Result<bool> {
    let mut changed = false;
    with_store(paths, |store| {
        if store.boot_id.as_deref() == Some(current) {
            return;
        }
        changed = store.boot_id.is_some();
        let first = store.boot_id.is_none();
        store.boot_id = Some(current.to_owned());
        if first {
            return;
        }
        for s in store.sessions.values_mut() {
            // Processes of the previous boot are gone; whatever paused recovery is moot.
            s.live_pid = None;
            s.live_start = None;
            s.live_boot = None;
            s.misses = 0;
            s.suppressed = false;
            s.boot_due = (s.mode() != Mode::Disabled && s.boot_started.as_deref() != Some(current))
                .then(|| current.to_owned());
        }
    })?;
    Ok(changed)
}

/// Compare the array start epoch with the persisted one (same boot only; a new boot already
/// made every session due). A different epoch makes every session with a mode that has not
/// been started for it due an array-start start; an unchanged one (a supervisor restart,
/// a cutover) changes nothing; the very first pass only records it.
fn detect_array(paths: &Paths, current: &str, boot_changed: bool) -> Result<()> {
    with_store(paths, |store| {
        if store.array_id.as_deref() == Some(current) {
            return;
        }
        let first = store.array_id.is_none();
        store.array_id = Some(current.to_owned());
        if first || boot_changed {
            return;
        }
        for s in store.sessions.values_mut() {
            s.live_pid = None;
            s.live_start = None;
            s.live_boot = None;
            s.misses = 0;
            s.suppressed = false;
            s.array_due = (s.mode() != Mode::Disabled
                && s.array_started.as_deref() != Some(current))
            .then(|| current.to_owned());
        }
    })
}

/// How a vanished process ended.
#[derive(Debug, PartialEq, Eq)]
enum Exit {
    /// Killed by a crash signal: start again.
    Crash(String),
    /// An orderly exit, or no sign of a crash: nothing to do.
    Clean(String),
    /// Cannot be told reliably: nothing to do (unknown means no start).
    Unclassified(String),
}
/// Crash evidence for `pid` in a kernel log: an OOM kill (SIGKILL), a segfault (SIGSEGV)
/// or a fatal CPU trap (SIGILL, SIGSEGV, SIGFPE). Lines older than the process (`start`
/// in clock ticks since boot) belong to an earlier user of the pid.
fn kernel_evidence(log: &str, pid: u32, start: u64) -> Option<String> {
    let oom = format!("Killed process {pid} (");
    let segv = format!("[{pid}]: segfault at");
    let trap = format!("[{pid}] trap ");
    let general = format!("[{pid}] general protection");
    log.lines().find_map(|line| {
        let stamp = line
            .strip_prefix('[')
            .and_then(|l| l.split_once(']'))
            .and_then(|(t, _)| t.trim().parse::<f64>().ok());
        // Ticks are 1/100 s (USER_HZ); one second of slack.
        if start > 0 && stamp.is_some_and(|t| t * 100.0 + 100.0 < start as f64) {
            return None;
        }
        if line.contains("Out of memory") && line.contains(&oom)
            || line.contains("out of memory") && line.contains(&oom)
        {
            Some("killed by the kernel OOM killer (SIGKILL)".to_owned())
        } else if line.contains(&segv) {
            Some("SIGSEGV (segfault)".to_owned())
        } else if line.contains(&trap) || line.contains(&general) {
            Some("fatal CPU trap (SIGILL/SIGSEGV/SIGFPE)".to_owned())
        } else {
            None
        }
    })
}
/// Decide how the process last seen running ended. Sources, in order: the kernel log
/// (any provider); for Claude the registry entry `sessions/<pid>.json`, which an orderly
/// shutdown removes. HERDR exposes no pane exit status and the other providers no exit
/// marker, so everything else is `Unclassified`.
fn classify(paths: &Paths, s: &Session, setting: &Setting, log: Option<&str>) -> Exit {
    let Some(pid) = setting.live_pid else {
        return Exit::Unclassified("no process was recorded".into());
    };
    if let Some(what) = log.and_then(|l| kernel_evidence(l, pid, setting.live_start.unwrap_or(0))) {
        return Exit::Crash(what);
    }
    if native::of(&s.provider) == Some(ProviderId::Claude) {
        let entry = native::account(paths, ProviderId::Claude, &s.account)
            .ok()
            .map(|a| a.home.join("sessions").join(format!("{pid}.json")));
        return match entry {
            Some(path) if path.exists() => Exit::Unclassified(format!(
                "process {pid} left its registry entry behind but no kernel record of a crash signal exists (a hard kill and a closed pane look alike)"
            )),
            Some(_) => Exit::Clean(format!(
                "process {pid} shut down in order (its registry entry was removed) and the kernel log shows no crash"
            )),
            None => Exit::Unclassified("the account home is unavailable".into()),
        };
    }
    Exit::Unclassified(format!(
        "{} exposes no exit status and the kernel log shows no crash of process {pid}",
        s.provider
    ))
}

/// What started a session.
enum Trigger {
    Boot,
    Array,
    Crash(String),
}

// ------------------------------------------------------------------------ tick

/// One pass of the supervisor loop. Cheap when no session has the setting on.
pub async fn tick(paths: &Paths) -> Result<Vec<Value>> {
    tick_with(paths, &Normal).await
}
pub(crate) async fn tick_with<S: Sender>(paths: &Paths, sender: &S) -> Result<Vec<Value>> {
    migrate(paths)?;
    // Before the early return: the boot must be recorded even while no session has a mode,
    // or a session enabled later would look like it was due a boot start.
    let boot = boot_id();
    let array = array_id();
    let boot_changed = match &boot {
        Some(b) => detect_boot(paths, b)?,
        None => false,
    };
    if let Some(a) = &array {
        detect_array(paths, a, boot_changed)?;
    }
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
                s.set_mode(Mode::Disabled);
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
            Status::Live(pane, proc) => {
                note_live(paths, &s, &setting, pane, proc, boot.as_deref());
                if setting.array_due.is_some() {
                    update(paths, &s.id, |x| {
                        x.array_started = x.array_due.take();
                    });
                }
            }
            Status::Down => {
                if setting.suppressed {
                    continue;
                }
                let due_boot =
                    boot.is_some() && setting.boot_due == boot && setting.boot_started != boot;
                let due_array =
                    array.is_some() && setting.array_due == array && setting.array_started != array;
                let was_running =
                    boot.is_some() && setting.live_boot == boot && setting.live_pid.is_some();
                // Never seen running on this boot and not due a boot start (enabled while
                // stopped, or explicitly stopped): nothing can start it now.
                if !due_boot && !due_array && !was_running {
                    continue;
                }
                // Settle: the same number of consecutive checks as ever, so discovery
                // (and a HERDR that is still coming up) has time to catch up.
                let misses = (setting.misses + 1).min(MISS_THRESHOLD);
                if misses != setting.misses {
                    update(paths, &s.id, |x| x.misses = misses);
                }
                if misses < MISS_THRESHOLD {
                    continue;
                }
                let trigger = if due_boot {
                    Trigger::Boot
                } else if due_array {
                    Trigger::Array
                } else {
                    match classify(paths, &s, &setting, kernel_log().as_deref()) {
                        Exit::Crash(what) => Trigger::Crash(what),
                        Exit::Clean(detail) => {
                            skip_exit(paths, &s.id, "skipped_clean_exit", "CLEAN_EXIT", &detail);
                            continue;
                        }
                        Exit::Unclassified(detail) => {
                            skip_exit(
                                paths,
                                &s.id,
                                "exit_unclassified",
                                "EXIT_UNCLASSIFIED",
                                &detail,
                            );
                            continue;
                        }
                    }
                };
                if let Some(op) = attempt(
                    paths,
                    &s,
                    &setting,
                    &trigger,
                    (boot.as_deref(), array.as_deref()),
                    sender,
                )
                .await
                {
                    outcomes.push(op);
                }
            }
        }
    }
    Ok(outcomes)
}

/// A vanished process that starts nothing: record why, once, and stop watching that pid.
fn skip_exit(paths: &Paths, id: &str, state: &str, code: &str, detail: &str) {
    update(paths, id, |s| {
        s.live_pid = None;
        s.live_start = None;
        s.live_boot = None;
        s.misses = 0;
        s.last_result = Some(result(state, code, detail, None));
    });
}

fn note_live(
    paths: &Paths,
    s: &Session,
    setting: &Setting,
    pane: Option<String>,
    proc: Proc,
    boot: Option<&str>,
) {
    let moved = pane.is_some() && pane != setting.last_pane;
    let process_changed = boot.is_some()
        && proc.is_some()
        && (setting.live_pid != proc.map(|p| p.0)
            || setting.live_start != proc.map(|p| p.1)
            || setting.live_boot.as_deref() != boot);
    let boot_pending = setting.boot_due.is_some() || setting.stopped_explicitly_at.is_some();
    if !moved && !process_changed && !boot_pending && setting.misses == 0 && !setting.suppressed {
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
        // Running: no boot start is needed (it already runs for this boot), and any
        // earlier explicit stop is over.
        if let Some(b) = boot {
            if x.boot_due.is_some() {
                x.boot_started = Some(b.to_owned());
            }
            x.boot_due = None;
            if let Some((pid, start)) = proc {
                x.live_pid = Some(pid);
                x.live_start = Some(start);
                x.live_boot = Some(b.to_owned());
            }
        }
        x.stopped_explicitly_at = None;
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
    trigger: &Trigger,
    (boot, array): (Option<&str>, Option<&str>),
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
        retire(paths, &s.id);
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
    let prompt = setting.prompt.clone().unwrap_or_else(|| match trigger {
        Trigger::Boot => default_prompt(boot_time(), None),
        Trigger::Array => array_prompt(now),
        Trigger::Crash(what) => default_prompt(now, Some(what)),
    });
    let (trigger_name, trigger_detail) = match trigger {
        Trigger::Boot => (
            "boot",
            "The host booted; the session was not running.".to_owned(),
        ),
        Trigger::Array => (
            "array_start",
            "The array was started; the session was not running.".to_owned(),
        ),
        Trigger::Crash(what) => ("crash", what.clone()),
    };
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
        mode: setting.mode().as_str().into(),
        prompt,
        message_id: uuid::Uuid::new_v4().to_string(),
        prompt_state: if setting.mode() == Mode::Start {
            "skipped".into()
        } else {
            "pending".into()
        },
        prompt_attempts: 0,
        prompt_detail: String::new(),
        resume: "pending".into(),
        phase: "admitted".into(),
        detail: format!("Admitted ({trigger_name}): {trigger_detail}"),
        trigger: trigger_name.into(),
        uncertain_from: None,
        generation_after: None,
        created_at: now,
        updated_at: now,
    };
    save(paths, &mut r).ok()?;
    let id = s.id.clone();
    let boot = boot.map(str::to_owned);
    let array = array.map(str::to_owned);
    let _ = with_store(paths, |store| {
        if let Some(setting) = store.sessions.get_mut(&id) {
            setting.recoveries.retain(|t| now - *t < HOUR);
            setting.recoveries.push(now);
            // One start per boot and one per observed process, recorded at admission.
            setting.live_pid = None;
            setting.live_start = None;
            setting.live_boot = None;
            setting.stopped_explicitly_at = None;
            if boot.is_some() {
                setting.boot_started = boot;
                setting.boot_due = None;
            }
            if array.is_some() {
                setting.array_started = array;
                setting.array_due = None;
            }
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
