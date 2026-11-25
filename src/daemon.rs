use crate::{
    config::Config,
    model::{Account, Credits, State, Usage, Window},
    paths::{Paths, private_dir},
    provider::{
        ProviderId,
        ops::{self, Enrollment, LiveSteering, Logout},
    },
    rpc::{Rpc, read_frame},
    store::{Store, load_config, save_config},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

struct Request {
    value: Value,
    reply: oneshot::Sender<Value>,
    delivered: oneshot::Receiver<()>,
}
struct Poll {
    name: String,
    identity: Option<String>,
    usage: Option<Usage>,
    unauthenticated: bool,
    /// Why no reading came back (`None` when one did, or the account is unauthenticated).
    failure: Option<String>,
}

/// Contract 1.62.1: an on-demand Codex meter read is skipped while the cached reading is
/// younger than this (the Claude read's period); `SESSIONSATION_CODEX_METER_CACHE_SECONDS`
/// overrides it for tests.
pub const CODEX_METER_CACHE_SECONDS: i64 = 60;
pub const CODEX_METER_CACHE_ENV: &str = "SESSIONSATION_CODEX_METER_CACHE_SECONDS";
/// A rate-limits read goes to the provider's backend; the shared app-server's 3 s control
/// default is too short for it.
const RATE_LIMITS_TIMEOUT: Duration = Duration::from_secs(20);

/// Why the newest read of a Codex account did not produce a reading, by account name.
/// In memory only: the supervisor is the only reader (`status`), and a restart starts clean.
static METER_NOTES: std::sync::Mutex<BTreeMap<String, String>> =
    std::sync::Mutex::new(BTreeMap::new());
fn set_meter_note(name: &str, note: Option<String>) {
    if let Ok(mut notes) = METER_NOTES.lock() {
        match note {
            Some(note) => notes.insert(name.to_owned(), note),
            None => notes.remove(name),
        };
    }
}
fn meter_note(name: &str) -> Option<String> {
    METER_NOTES.lock().ok()?.get(name).cloned()
}
/// What the supervisor says when it is not reading an account's meter.
fn pause_sentence(code: &str) -> &'static str {
    match code {
        "active_session" => {
            "not read: a session handoff to this account may have a terminal open on it, and a read could disturb that terminal's app-server"
        }
        _ => {
            "not read: a running session holds this account's lease and no shared app-server serves it, so a second app-server could contend with it for the login"
        }
    }
}
/// A Codex meter that is not fresh says why. A fresh reading never gets a reason.
pub fn explain_stale_meter(meter: &mut Value, reason: Option<String>) {
    if meter["supported"] != true || meter["fresh"] != false {
        return;
    }
    let never = meter["reason"].as_str().map(str::to_owned);
    if never.is_some() && reason.is_none() {
        return;
    }
    let why = reason.unwrap_or_else(|| {
        format!(
            "the last reading is {} s old and no newer read has succeeded",
            meter["age_seconds"].as_i64().unwrap_or(0)
        )
    });
    meter["reason"] = json!(match never {
        Some(never) => format!("{never}; {why}"),
        None => why,
    });
}
pub fn codex_stale_reason(paths: &Paths, name: &str) -> Option<String> {
    monitoring_pause_reason(paths, name)
        .map(|code| pause_sentence(code).to_owned())
        .or_else(|| meter_note(name))
}

pub fn parse_usage(value: &Value, at: i64) -> Result<Usage> {
    let limits = if value["rateLimitsByLimitId"][crate::provider::codex::USAGE_BUCKET].is_object() {
        &value["rateLimitsByLimitId"][crate::provider::codex::USAGE_BUCKET]
    } else {
        &value["rateLimits"]
    };
    ensure!(limits.is_object(), "missing usage limits");
    if let Some(id) = limits["limitId"].as_str() {
        ensure!(
            id == crate::provider::codex::USAGE_BUCKET,
            "unexpected usage bucket"
        );
    }
    let mut usage = Usage {
        observed_at: at,
        plan_type: limits["planType"].as_str().map(str::to_owned),
        reported_windows: (limits.get("primary").is_some() && limits.get("secondary").is_some())
            .then(Vec::new),
        exhausted: !limits["rateLimitReachedType"].is_null(),
        reached_type: limits["rateLimitReachedType"].as_str().map(str::to_owned),
        spend_control_reached: limits["spendControlReached"].as_bool(),
        ordinary_usage_allowed: value["ordinaryUsageAllowed"].as_bool(),
        reset_credits: crate::reset_credit::parse_summary(&value["rateLimitResetCredits"], at),
        ..Default::default()
    };
    if let Some(raw) = limits["credits"].as_object() {
        usage.credits = Some(Credits {
            has_credits: raw
                .get("hasCredits")
                .and_then(Value::as_bool)
                .context("missing credit availability")?,
            unlimited: raw
                .get("unlimited")
                .and_then(Value::as_bool)
                .context("missing unlimited credit flag")?,
            balance: raw
                .get("balance")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    for field in ["primary", "secondary"] {
        let raw = &limits[field];
        if raw.is_null() {
            continue;
        }
        let used = raw["usedPercent"].as_f64().context("missing percentage")?;
        ensure!(
            used.is_finite() && (0. ..=100.).contains(&used),
            "invalid percentage"
        );
        let duration = raw["windowDurationMins"]
            .as_u64()
            .context("missing window duration")?;
        if let Some(windows) = &mut usage.reported_windows {
            windows.push(duration);
        }
        let window = Window {
            used_percent: used,
            duration_minutes: duration,
            resets_at: raw["resetsAt"].as_i64().context("missing reset time")?,
        };
        // Do not silently treat another window as a five-hour or weekly allowance.
        match duration {
            300 => usage.short = Some(window),
            10080 => usage.weekly = Some(window),
            _ => {}
        }
    }
    Ok(usage)
}

/// The account named by `params.name`, verifying `params.provider` when given.
fn named<'a>(state: &'a State, params: &Value) -> Result<&'a Account> {
    let name = params["name"].as_str().context("missing account name")?;
    let provider = params["provider"]
        .as_str()
        .map(ProviderId::parse)
        .transpose()?;
    state.resolve_account(name, provider)
}
/// The registry key of the account a request names.
fn key_of(state: &State, params: &Value) -> Result<String> {
    Ok(named(state, params)?.key())
}
/// Supervisor operations that own a Codex app-server/home.
fn codex_named<'a>(state: &'a State, params: &Value) -> Result<&'a Account> {
    // Codex-only methods predate providers: an absent provider means Codex.
    let name = params["name"].as_str().context("missing account name")?;
    let provider = params["provider"]
        .as_str()
        .map(ProviderId::parse)
        .transpose()?
        .unwrap_or(ProviderId::Codex);
    let account = state.resolve_account(name, Some(provider))?;
    ops::require(
        ops::facts(account.provider).supervised,
        format_args!(
            "{} accounts do not support this operation",
            account.provider
        ),
    )?;
    Ok(account)
}
/// Display names follow account-name characters, up to 64 bytes (ACCOUNT_DISPLAY_NAME_MAX).
pub fn validate_display_name(display: &str) -> Result<()> {
    ensure!(
        display.len() <= crate::paths::ACCOUNT_DISPLAY_NAME_MAX
            && crate::paths::validate_name(display).is_ok(),
        "INVALID_DISPLAY_NAME: use 1-64 ASCII letters, digits, hyphens or underscores"
    );
    Ok(())
}
/// While a Claude account awaits login, re-probe `auth status` this often so a
/// completed interactive login is reflected within seconds.
pub const CLAUDE_AUTH_REQUIRED_POLL_SECONDS: i64 = 3;
struct ClaudePoll {
    name: String,
    problem: Option<String>,
    plan: Option<crate::plan::PlanObservation>,
}

fn monitoring_pause_reason(paths: &Paths, name: &str) -> Option<&'static str> {
    if paths
        .account(name)
        .is_ok_and(|home| crate::control::available(&home))
    {
        return None;
    }
    if crate::external::destination_tui_may_be_active(paths, name) {
        return Some("active_session");
    }
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.account(name).ok()?.join(".switcher-lease"))
        .ok()?;
    if lease.try_lock_exclusive().is_err() {
        return (!crate::external::inactive_source_only_lease(paths, name))
            .then_some("account_lease");
    }
    let _ = FileExt::unlock(&lease);
    None
}

/// Stores a plan observation; true when the saved record should be rewritten (the
/// value changed, or the stored observation time is an hour old). Otherwise only
/// the in-memory observation time advances.
fn record_plan(
    slot: &mut Option<crate::plan::PlanObservation>,
    new: &crate::plan::PlanObservation,
) -> bool {
    match slot {
        Some(old)
            if old.raw == new.raw
                && old.rate_limit_tier == new.rate_limit_tier
                && new.verified_at - old.verified_at < 3600 =>
        {
            old.verified_at = old.verified_at.max(new.verified_at);
            false
        }
        _ => {
            *slot = Some(new.clone());
            true
        }
    }
}

/// Public `plan` object of an account (contract 1.46.7): Claude's last `auth status`
/// observation, Codex's last usage snapshot, null for Gemini and Antigravity.
pub fn account_plan(a: &Account, at: i64) -> Value {
    crate::provider::ops::ops(a.provider).account_plan(a, at)
}

/// Provider usage in the shared account shape plus `source`/`semantics`.
/// Codex values are the supervisor's snapshot; Claude values are projected from
/// the statusline recorder; Gemini reports no usage (state `unsupported`). Returns (usage, usage_fresh, usage_state).
pub(crate) fn account_usage(
    paths: &Paths,
    a: &Account,
    config: &Config,
    at: i64,
) -> (Value, bool, String) {
    crate::provider::ops::ops(a.provider).account_usage(paths, a, config, at)
}

pub fn status(paths: &Paths, state: &State, config: &Config, version: &str) -> Value {
    let at = now();
    let accounts: Vec<_> = state.ordered().into_iter().map(|a| {
        let (mut usage, fresh, usage_state) = account_usage(paths, a, config, at);
        // Contract 1.62.0 (SM8): one meter shape for every provider.
        usage["meter"] = crate::provider::usage_meter::of(a.provider, &usage, at, config.stale_seconds);
        let supervised = ops::facts(a.provider).supervised;
        if supervised && usage["meter"]["fresh"] == false {
            explain_stale_meter(&mut usage["meter"], codex_stale_reason(paths, &a.name));
        }
        json!({
        "name":a.name,"folder_name":a.name,"name_mismatch":a.display_name.is_some(),"generation":crate::accounts::account_generation(a),
        "order":a.order,"display_name":a.display_name,"provider":a.provider,"config_home":a.config_home,
        "enabled":a.enabled,"auth_required":a.auth_required,"auth_detail":a.auth_detail,"deletion_id":a.deletion_id,
        "switch_thresholds":a.switch_thresholds,
        "usage":usage,"usage_fresh":fresh,"usage_state":usage_state,"plan":account_plan(a,at),
        "credit_fresh":supervised && a.usage.credits.is_some() && a.usage.observed_fresh(at,config.stale_seconds),
        "monitoring_pause_reason":supervised.then(||monitoring_pause_reason(paths,&a.name)).flatten(),
        "eligible":supervised && eligible(a,config,at),
        "assigned_sessions":state.sessions.values().filter(|s|supervised && s.account.as_ref()==Some(&a.name)).count()
    })}).collect();
    let sessions: Vec<_> = state.sessions.values().map(|s|json!({"id":s.id,"thread_id":s.root_thread,"cwd":s.cwd,"account":s.account,"phase":s.phase,"reason":s.reason,"tmux_pane":s.tmux_pane,"manual_pause":s.manual_pause,"resume_work":s.resume_work})).collect();
    json!({"schema_version":1,"daemon":{"running":true,"package_version":env!("CARGO_PKG_VERSION"),"api_version":1,"lifecycle_reservations":["message-recoveries","space-moves"]},"codex_version":version,
        "capabilities":{"account_deletion":true,"account_monitoring":true,"manual_exit_handoff":true,"manual_moves":true,"explicit_account_launch":true,"session_inventory":true,"session_open":true,"archive_recovery":true,"project_relocation":true,"session_restart":true,"session_termination":true,"session_pause":true,"job_retry":true,"verified_paginated_moves":true,"managed_launch":false,"experimental_auto_start":false,"experimental_auto_switching":false,"automatic_switching":false,"conversation_import":false},
        "readiness":{"ready":false,"code":"NATIVE_HANDOFF_UNVERIFIED","detail":"Automatic account switching is not available."},
        "automatic_switch_preferences":state.automatic_switching,
        "automatic_start_preferences":state.automatic_start,
        "accounts":accounts,"sessions":sessions,"settings":config,"observed_at":at})
}

async fn poll(name: String, rpc: Rpc, home: std::path::PathBuf) -> Poll {
    let account = rpc
        .request("account/read", json!({"refreshToken":false}))
        .await;
    let unauthenticated = account.as_ref().is_ok_and(|a| a["account"].is_null());
    let identity = if account
        .as_ref()
        .is_ok_and(|a| a["account"]["type"] == "chatgpt")
    {
        crate::identity::read_identity(&home).ok()
    } else {
        None
    };
    let mut failure = None;
    let usage = if identity.is_some() {
        match rpc
            .request_timeout("account/rateLimits/read", json!({}), RATE_LIMITS_TIMEOUT)
            .await
        {
            Ok(v) => match parse_usage(&v, now()) {
                Ok(usage) => Some(usage),
                Err(e) => {
                    failure = Some(format!("the rate-limits answer could not be read: {e}"));
                    None
                }
            },
            Err(e) => {
                failure = Some(format!("the rate-limits read failed: {e}"));
                None
            }
        }
    } else {
        if !unauthenticated {
            failure = Some(match &account {
                Err(e) => format!("the app-server did not answer: {e}"),
                Ok(_) => "the account identity could not be read".into(),
            });
        }
        None
    };
    Poll {
        name,
        identity,
        usage,
        unauthenticated,
        failure,
    }
}

/// Record one finished account read; true when a usage reading was stored.
fn apply_poll(state: &mut State, store: &mut Store, result: Poll) -> Result<bool> {
    let duplicate = result.identity.as_ref().is_some_and(|identity| {
        state
            .accounts
            .values()
            .any(|a| a.key() != result.name && &a.identity == identity)
    });
    let mut stored = false;
    set_meter_note(&result.name, result.failure.clone());
    if let Some(a) = state.accounts.get_mut(&result.name) {
        if duplicate
            || result
                .identity
                .as_ref()
                .is_some_and(|id| !a.identity.is_empty() && id != &a.identity)
        {
            a.auth_required = true;
            a.enabled = false;
        } else if let Some(identity) = result.identity {
            a.identity = identity;
            a.auth_required = false;
            if let Some(usage) = result.usage {
                a.observe(usage);
                stored = true;
            }
        } else if result.unauthenticated {
            a.auth_required = true;
        }
        store.save(state, now(), "account_observed", &result.name)?;
    }
    Ok(stored)
}

pub async fn serve(paths: Paths) -> Result<()> {
    std::env::set_current_dir(&paths.root)?;
    // Root alias (contract 1.54.0): old-spelling runtime paths reach this runtime.
    for link in crate::alias::runtime::ensure(&paths) {
        if link["result"]
            .as_str()
            .is_some_and(|r| r.starts_with("refused"))
        {
            eprintln!("root alias: runtime compatibility path not maintained: {link}");
        }
    }
    // A crash between create and rename of an atomic write leaves a `.tmp*` file.
    crate::paths::sweep_stale_temps(&paths.root, crate::paths::STALE_TEMP_AGE);
    // Receipts schema (contract 1.66.1): migrated here once, never on the read path.
    if let Err(e) = crate::messaging::prepare_ledger(&paths) {
        eprintln!("receipts ledger: schema not prepared at start ({e:#}); the first open migrates");
    }
    // Must precede removing the stale socket and opening the database.
    let mut store = Store::open(&paths)?;
    let mut state = store.load()?;
    if !state.normalize_order().is_empty() {
        store.save(&state, now(), "account.order_migrated", "")?;
    }
    let mut config = load_config(&paths)?;
    // Data-root retention (logs, cache, operations history, receipts): first
    // pass a few minutes after start, then hourly. Housekeeping never blocks.
    let retention_paths = paths.clone();
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(300);
        loop {
            tokio::time::sleep(delay).await;
            delay = Duration::from_secs(3600);
            let p = retention_paths.clone();
            match tokio::task::spawn_blocking(move || crate::retention::run(&p, false)).await {
                Ok(Ok(summary)) => eprintln!("retention: {summary}"),
                Ok(Err(e)) => eprintln!("retention failed: {e:#}"),
                Err(e) => eprintln!("retention failed: {e}"),
            }
        }
    });
    // Keep every enrolled Codex account's app-server on a deliberately selected
    // release; a running updater exits at its next check (no restart needed).
    for (name, account) in &state.accounts {
        if !ops::facts(account.provider).supervised || account.deletion_id.is_some() {
            continue;
        }
        let pinned = paths.account(name).and_then(|home| {
            if home.is_dir() {
                crate::provider::codex::pin_daemon_release(&home)
            } else {
                Ok(false)
            }
        });
        if let Err(e) = pinned {
            eprintln!("codex daemon release not pinned for {name}: {e:#}");
        }
    }
    // Contract 1.66.3: the Codex version (status and reset-credit messages only) is
    // probed beside startup, never before the socket serves: a slow `codex --version`
    // held the 1.66.2 cutover's socket for 9.8 s. Up to half a second is awaited
    // below; after that status says "unavailable" until the probe answers.
    let codex_binary = config.codex_binary.clone();
    let mut version_probe = tokio::spawn(async move {
        codex_version(&codex_binary)
            .await
            .unwrap_or_else(|_| "unavailable".into())
    });
    let socket = paths.control();
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let (requests_tx, mut requests) = mpsc::channel::<Request>(64);
    let accept_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            // Same-user Unix socket only. Parent directory and socket are private too.
            if !stream
                .peer_cred()
                .is_ok_and(|c| c.uid() == unsafe { libc::geteuid() })
            {
                continue;
            }
            let tx = requests_tx.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.split();
                let mut reader = BufReader::new(read);
                let Ok(Ok(Some(value))) =
                    tokio::time::timeout(Duration::from_secs(10), read_frame(&mut reader)).await
                else {
                    return;
                };
                let (reply, rx) = oneshot::channel();
                let (written, delivered) = oneshot::channel();
                if tx
                    .send(Request {
                        value,
                        reply,
                        delivered,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                if let Ok(v) = rx.await {
                    if let Ok(mut bytes) = serde_json::to_vec(&v) {
                        bytes.push(b'\n');
                        let _ = write.write_all(&bytes).await;
                    }
                }
                let _ = written.send(());
            });
        }
    });
    // A restart (cutover) must not leave `state --cached` on the previous process's
    // last snapshot: write a fresh one as soon as the socket serves requests.
    let snapshot_paths = paths.clone();
    tokio::spawn(async move {
        tokio::time::sleep(crate::timing::secs(2)).await;
        if let Err(e) =
            crate::api::state_filtered(&snapshot_paths, crate::api::ArchiveFilter::Active).await
        {
            eprintln!("startup state snapshot failed: {e:#}");
        }
    });
    let auto_recover_task = tokio::spawn(crate::auto_recover::monitor(paths.clone()));
    let compaction_task = tokio::spawn(crate::messaging::compaction::monitor(paths.clone()));
    // Contract 1.49.0: model changes and one-pass launches.
    let model_task = tokio::spawn(crate::messaging::model_change::monitor(paths.clone()));
    // Contract 1.64.0: one re-dispatch of an unsent request whose worker died.
    let message_worker_task = tokio::spawn(crate::messaging::claude_steer::monitor(paths.clone()));
    // Contract 1.62.0 (SM8): idle Claude meters are read in the background.
    let meter_task = tokio::spawn(crate::provider::claude::meter::monitor(paths.clone()));
    // Contract 1.64.0 (SM5): Claude hook lines move into the session event journal.
    let events_task = tokio::spawn(crate::provider::events::monitor(paths.clone()));
    // Contract 1.62.1: `state --cached` stays fresh with no caller of full `state`.
    let snapshot_task = tokio::spawn(crate::api::snapshot_monitor(paths.clone()));
    let (history_tx, mut history_rx) = mpsc::channel(1);
    let history_task = tokio::spawn(crate::usage_history::monitor(paths.clone(), history_tx));
    let mut history = crate::usage_history::Recorder::default();
    let mut helpers = BTreeMap::<String, Rpc>::new();
    let mut pending = BTreeSet::new();
    let mut last_poll = BTreeMap::<String, i64>::new();
    let (poll_tx, mut polls) = mpsc::channel::<Poll>(64);
    let (claude_tx, mut claude_polls) = mpsc::channel::<ClaudePoll>(64);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let (mut version, mut probing) = match tokio::time::timeout(
        crate::timing::scaled(Duration::from_millis(500)),
        &mut version_probe,
    )
    .await
    {
        Ok(probed) => (probed.unwrap_or_else(|_| "unavailable".into()), false),
        Err(_) => ("unavailable".to_owned(), true),
    };
    let mut stopping = false;
    loop {
        tokio::select! {
            probed = &mut version_probe, if probing => {
                probing = false;
                version = probed.unwrap_or_else(|_| "unavailable".into());
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            Some(request) = requests.recv() => {
                let method = request.value["method"].as_str().unwrap_or("");
                let params = &request.value["params"];
                let result: Result<Value> = async {
                    match method {
                        "status" => Ok(status(&paths,&state,&config,&version)),
                        "account/history" => {
                            let key = key_of(&state,params)?; let name = key.as_str();
                            let account = state.accounts.get(name).context("unknown account")?;
                            let start = params["start"].as_i64().context("missing range start")?;
                            let end = params["end"].as_i64().context("missing range end")?;
                            let step = params["step"].as_i64().unwrap_or(1800);
                            Ok(serde_json::to_value(crate::usage_history::query(&store.db,name,&account.usage,start,end,step)?)?)
                        }
                        "account/thresholds" => {
                            let key = key_of(&state,params)?; let name = key.as_str();
                            let account = state.accounts.get_mut(name).context("unknown account")?;
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let mut thresholds = account.switch_thresholds.clone();
                            thresholds.observe_plan(account.usage.plan_type.as_deref());
                            if let Some(multiplier) = params.get("multiplier") {thresholds.set_multiplier(multiplier.as_f64().context("invalid plan ratio")?)?;}
                            if params.get("short").is_some() || params.get("weekly").is_some() {
                                thresholds.set_percentages(params["short"].as_f64().context("missing 5-hour used percentage")?,
                                    params["weekly"].as_f64().context("missing weekly used percentage")?)?;
                            }
                            account.switch_thresholds = thresholds.clone();
                            store.save(&state,now(),"thresholds_updated",name)?;
                            Ok(json!({"saved":true,"thresholds":thresholds}))
                        }
                        "daemon/stop" => {
                            ensure!(state.sessions.is_empty(), "cannot stop while managed sessions need reconciliation");
                            stopping = true; Ok(json!({"stopping":true}))
                        }
                        // Contract 1.62.1: read one Codex account's meter now when the cached
                        // reading is older than the cache period and a read is safe. The answer
                        // says what happened; it never fails because a read was not possible.
                        "account/usage_read" => {
                            let account = codex_named(&state,params)?;
                            let name = account.name.clone();
                            let cache = std::env::var(CODEX_METER_CACHE_ENV).ok().and_then(|v|v.parse::<i64>().ok()).unwrap_or(CODEX_METER_CACHE_SECONDS);
                            let age = now() - account.usage.observed_at;
                            if !account.enabled || account.deletion_id.is_some() {
                                Ok(json!({"attempted":false,"ok":false,"reason":"the account is disabled or being removed"}))
                            } else if (0..cache).contains(&age) {
                                Ok(json!({"attempted":false,"ok":true,"reason":null}))
                            } else if let Some(code) = monitoring_pause_reason(&paths,&name) {
                                let reason = pause_sentence(code);
                                set_meter_note(&name, Some(reason.to_owned()));
                                Ok(json!({"attempted":false,"ok":false,"reason":reason}))
                            } else if pending.contains(&name) {
                                Ok(json!({"attempted":false,"ok":false,"reason":"a background read of this account is still running"}))
                            } else {
                                last_poll.insert(name.clone(),now());
                                if let Some(helper) = helpers.get(&name) {
                                    if !helper.is_running().await {helpers.remove(&name);}
                                }
                                if !helpers.contains_key(&name) {
                                    match Rpc::spawn(&config.codex_binary,&paths.account(&name)?,&paths.root).await {
                                        Ok(rpc) => {helpers.insert(name.clone(),rpc);},
                                        Err(e) => {
                                            let reason = format!("the app-server could not be reached or started: {e}");
                                            set_meter_note(&name, Some(reason.clone()));
                                            return Ok(json!({"attempted":true,"ok":false,"reason":reason}));
                                        }
                                    }
                                }
                                let rpc = helpers[&name].clone(); let home = paths.account(&name)?;
                                let polled = tokio::time::timeout(Duration::from_secs(40), poll(name.clone(),rpc,home)).await;
                                match polled {
                                    Ok(result) => {
                                        let failure = result.failure.clone();
                                        let stored = apply_poll(&mut state,&mut store,result)?;
                                        Ok(json!({"attempted":true,"ok":stored,"reason":failure}))
                                    }
                                    Err(_) => {
                                        let reason = "the rate-limits read timed out";
                                        set_meter_note(&name, Some(reason.into()));
                                        Ok(json!({"attempted":true,"ok":false,"reason":reason}))
                                    }
                                }
                            }
                        }
                        "config/get" => Ok(serde_json::to_value(&config)?),
                        "config/set" => {
                            let (new, _) = Config::parse(params.clone())?; new.validate()?;
                            ensure!(new.codex_binary == config.codex_binary || helpers.is_empty(), "restart supervisor before changing an active Codex binary");
                            save_config(&paths,&new)?; config = new; Ok(json!({"saved":true}))
                        }
                        "account/delete/prepare" if matches!(params["provider"].as_str(),Some("claude"|"gemini"|"antigravity")) => {
                            let provider=named(&state,params)?.provider;
                            let name=params["name"].as_str().context("missing account name")?;
                            let id=params["request_id"].as_str().context("missing deletion request")?;
                            uuid::Uuid::parse_str(id)?;
                            let record=crate::account_deletion::status(&paths,id)?;
                            ensure!(record.account_id==format!("{provider}:{name}"),"ACCOUNT_DELETION_IDENTITY_MISMATCH");
                            let account=state.account(provider,name).context("ACCOUNT_NOT_FOUND")?;
                            ensure!(account.deletion_id.as_deref().is_none_or(|current|current==id),"ACCOUNT_DELETION_IN_PROGRESS");
                            ensure!(account.config_home==record.external_home,"ACCOUNT_DELETION_IDENTITY_MISMATCH"); // raw-path: the record copies config_home verbatim
                            let home=crate::provider::account_home(&paths,account)?;
                            crate::account_deletion::native_idle(&paths,provider,name,&home)?;
                            let account=state.account_mut(provider,name).unwrap();account.deletion_id=Some(id.into());account.enabled=false;
                            store.save(&state,now(),"account_deletion_reserved",name)?;
                            Ok(json!({"reserved":true}))
                        }
                        "account/delete/finish" if matches!(params["provider"].as_str(),Some("claude"|"gemini"|"antigravity")) => {
                            let name=params["name"].as_str().context("missing account name")?;
                            let id=params["request_id"].as_str().context("missing deletion request")?;
                            let provider=ProviderId::parse(params["provider"].as_str().unwrap_or_default())?;
                            let record=crate::account_deletion::status(&paths,id)?;
                            ensure!(record.account_id==format!("{provider}:{name}")&&record.phase=="deleting_local_data","ACCOUNT_DELETION_UNVERIFIED");
                            match &record.external_home {
                                Some(_) => ensure!(record.home_disposition.as_deref()==Some("left_in_place"),"ACCOUNT_DELETION_UNVERIFIED"),
                                None => {
                                    ensure!(record.history_preserved,"ACCOUNT_DELETION_UNVERIFIED");
                                    ensure!(!crate::provider::native::default_home(&paths,provider,name)?.exists(),"ACCOUNT_DATA_STILL_PRESENT");
                                }
                            }
                            if let Some(account)=state.account(provider,name){
                                ensure!(account.deletion_id.as_deref()==Some(id),"ACCOUNT_DELETION_IDENTITY_MISMATCH");
                            }
                            let key=crate::model::account_key(provider,name);
                            state.accounts.remove(&key);last_poll.remove(&key);pending.remove(&key);
                            store.save(&state,now(),"account_deleted",name)?;
                            Ok(json!({"deleted":true}))
                        }
                        "account/delete/prepare" => {
                            let name=params["name"].as_str().context("missing account name")?; codex_named(&state,params)?;
                            let id=params["request_id"].as_str().context("missing deletion request")?;
                            uuid::Uuid::parse_str(id)?;
                            let record=crate::account_deletion::status(&paths,id)?;
                            ensure!(record.account_id==format!("codex:{name}"),"ACCOUNT_DELETION_IDENTITY_MISMATCH");
                            let account=state.accounts.get(name).context("ACCOUNT_NOT_FOUND")?;
                            ensure!(account.deletion_id.as_deref().is_none_or(|current|current==id),"ACCOUNT_DELETION_IN_PROGRESS");
                            ensure!(!pending.contains(name),"ACCOUNT_POLL_IN_PROGRESS");
                            let home=paths.account(name)?;
                            let lease=OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW).open(home.join(".switcher-lease"))?;
                            ensure!(lease.try_lock_exclusive().is_err(),"ACCOUNT_DELETION_LEASE_REQUIRED");
                            crate::account_deletion::idle(&paths,&home).await?;
                            let account=state.accounts.get_mut(name).unwrap();account.deletion_id=Some(id.into());account.enabled=false;
                            store.save(&state,now(),"account_deletion_reserved",name)?;
                            if let Some(helper)=helpers.get(name){helper.stop_idle().await?;}helpers.remove(name);
                            Ok(json!({"reserved":true}))
                        }
                        "account/delete/finish" => {
                            let name=params["name"].as_str().context("missing account name")?;
                            let id=params["request_id"].as_str().context("missing deletion request")?;
                            let record=crate::account_deletion::status(&paths,id)?;
                            ensure!(record.account_id==format!("codex:{name}")&&record.history_preserved&&record.phase=="deleting_local_data","ACCOUNT_DELETION_UNVERIFIED");
                            ensure!(!paths.account(name)?.exists(),"ACCOUNT_DATA_STILL_PRESENT");
                            if let Some(account)=state.accounts.get(name){ensure!(account.deletion_id.as_deref()==Some(id),"ACCOUNT_DELETION_IDENTITY_MISMATCH");}
                            state.accounts.remove(name);helpers.remove(name);last_poll.remove(name);
                            store.save(&state,now(),"account_deleted",name)?;
                            Ok(json!({"deleted":true}))
                        }
                        "account/add" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            crate::paths::validate_new_account_name(name)?;
                            // Older clients predate providers and could only enroll Codex.
                            let provider = params["provider"].as_str().map(ProviderId::parse).transpose()?.unwrap_or(ProviderId::Codex);
                            if let Some(existing) = state.account(provider,name) { return Err(crate::provider::collision(existing)); }
                            if let Some(owner) = state.accounts.values().find(|a| a.provider == provider && a.display_name.as_deref() == Some(name)) { bail!("ACCOUNT_NAME_EXISTS: '{name}' is the display name of {}:{}; names and display names are unique per provider", owner.provider, owner.name); }
                            let key = crate::model::account_key(provider,name);
                            let config_dir = params["config_dir"].as_str().map(std::path::PathBuf::from);
                            let enrollment = ops::facts(provider).enrollment;
                            if let Enrollment::ManagedHome { create_home, auth_report } = enrollment {
                                ensure!(config_dir.is_none(), "INVALID_ARGUMENT: --config-dir is only supported for claude accounts");
                                let (home, existing) = create_home(&paths,name)?;
                                let auth = auth_report(&home);
                                let mut account = Account::with_provider(provider,name.into(),String::new());
                                account.auth_required = !auth["problem"].is_null();
                                account.auth_detail = auth["problem"].as_str().map(str::to_owned);
                                state.insert_account(account); last_poll.remove(&key);
                                store.save(&state,now(),"account_added",name)?;
                                return Ok(json!({"name":name,"provider":provider.as_str(),"account_id":format!("{provider}:{name}"),"home":home,"adopted":false,"existing_home":existing,"auth":auth,
                                    "login":{"action":"exec","interactive":true,"command":["sessionsation","account","login",format!("{provider}:{name}")]}}));
                            }
                            if let Enrollment::ClaudeHome = enrollment {
                                let adopted = config_dir.map(|dir| crate::provider::claude::validate_adopt(&paths,&state,&dir)).transpose()?;
                                // The managed home is the normal layout; an operator-populated one is registered as is.
                                let (home, existing) = match &adopted { Some(dir) => (dir.clone(), true), None => crate::provider::claude::create_home(&paths,name)? };
                                if adopted.is_none() && !existing { if let Err(e) = crate::provider::claude::bypass::default_new(&home) { eprintln!("bypass-warning default not set: {e}"); } }
                                if adopted.is_none() && !existing { if let Err(e) = crate::provider::claude::statusline_seed::default_new(&home,&crate::alias::logical_words(&config.claude_default_statusline_command)) { eprintln!("statusLine default not set: {e}"); } }
                                // Shared Claude state (1.39.0): link a managed home to the shared store when the store is enabled.
                                let shared_state = match &adopted { Some(_) => crate::shared_state::Ensure::NotApplicable.public(), None => match crate::shared_state::ensure_links(&paths,&home) { Ok(e) => e.public(), Err(e) => json!({"state":"error","error":format!("{e:#}")}) } };
                                let auth = crate::provider::claude::auth_report(&config,&home).await;
                                let mut account = Account::with_provider(ProviderId::Claude,name.into(),String::new());
                                account.config_home = adopted; account.auth_required = !auth["problem"].is_null();
                                account.auth_detail = auth["problem"].as_str().map(str::to_owned);
                                if auth["problem"].is_null() { account.plan = crate::plan::from_claude_auth(&auth, now()); }
                                state.insert_account(account); last_poll.remove(&key);
                                store.save(&state,now(),"account_added",name)?;
                                return Ok(json!({"name":name,"provider":"claude","account_id":format!("claude:{name}"),"home":home,"adopted":params["config_dir"].is_string(),"existing_home":existing,"auth":auth,"shared_state":shared_state,
                                    "login":{"action":"exec","interactive":true,"command":["sessionsation","account","login",format!("claude:{name}")]}}));
                            }
                            ensure!(config_dir.is_none(), "INVALID_ARGUMENT: --config-dir is only supported for claude accounts");
                            let home = paths.account(name)?; private_dir(&home)?;
                            let rpc = Rpc::spawn(&config.codex_binary,&home,&paths.root).await?;
                            let login = rpc.request("account/login/start",json!({"type":"chatgptDeviceCode"})).await;
                            match login {
                                Ok(login) => {
                                    let mut account = Account::new(name.into(),String::new()); account.auth_required = true;
                                    state.insert_account(account); helpers.insert(key.clone(),rpc);
                                    store.save(&state,now(),"account_added",name)?;
                                    Ok(json!({"name":name,"login":login}))
                                },
                                Err(e) => {rpc.stop_idle().await.ok(); Err(e)}
                            }
                        }
                        "account/login" => {
                            let name = named(&state,params)?.name.clone(); let name = name.as_str();
                            ops::require(ops::facts(state.accounts[name].provider).supervised, format_args!("Claude and Gemini login is interactive; run `sessionsation account login <provider>:{name}` in a terminal"))?;
                            ensure!(state.accounts[name].deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            ensure!(!pending.contains(name), "account poll is in progress; retry shortly");
                            if !helpers.contains_key(name) {let rpc = Rpc::spawn(&config.codex_binary,&paths.account(name)?,&paths.root).await?; helpers.insert(name.into(),rpc);}
                            let login = helpers[name].request("account/login/start",json!({"type":"chatgptDeviceCode"})).await?;
                            state.accounts.get_mut(name).unwrap().auth_required = true;
                            store.save(&state,now(),"account_login_started",name)?;
                            Ok(json!({"name":name,"login":login}))
                        }
                        "account/logout" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            let account = named(&state,params)?;
                            let provider = account.provider; let key = account.key();
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            ensure!(!pending.contains(&key), "ACCOUNT_POLL_IN_PROGRESS: account poll is in progress; retry shortly");
                            let home = crate::provider::account_home(&paths,account)?;
                            let was_required = account.auth_required;
                            let logout = ops::facts(provider).logout;
                            if let Logout::Refused(reason) = logout { bail!("{}", reason); }
                            crate::logout::ensure_idle(&paths,provider,name,&home).await?;
                            match logout {
                                Logout::CodexRpc => crate::logout::codex(&config,&home,helpers.get(&key)).await?,
                                Logout::ClaudeCli => crate::logout::claude(&config,&home).await?,
                                Logout::Refused(reason) => bail!("{}", reason),
                            }
                            let detail = crate::provider::native::is_native(provider).then(|| "AUTH_REQUIRED".to_owned());
                            let a = state.accounts.get_mut(&key).context("unknown account")?;
                            let changed = !was_required || a.auth_detail != detail;
                            a.auth_required = true; a.auth_detail = detail;
                            last_poll.insert(key.clone(), now());
                            if changed { store.save(&state,now(),"account.logged_out",name)?; }
                            Ok(json!({"account_id":format!("{provider}:{name}"),"provider":provider,"logged_out":true,"auth_required":true,"method":crate::logout::method(provider)}))
                        }
                        "account/live-steering" => {
                            let account = named(&state,params)?;
                            let provider = account.provider;
                            let steering = ops::facts(provider).live_steering;
                            ops::require(steering != LiveSteering::None, "live steering uses Claude Code peer messaging")?;
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let name = account.name.clone();
                            let home = crate::provider::account_home(&paths,account)?;
                            let (changed, previous) = match params["enable"].as_bool() {
                                Some(enable) => {
                                    let t = crate::provider::claude::peer::set(&home, enable)?;
                                    if t.changed { store.save(&state,now(),if enable {"account.live_steering_enabled"} else {"account.live_steering_disabled"},&name)?; }
                                    (t.changed, t.previous)
                                }
                                None => (false, crate::provider::claude::peer::inbound(&home)),
                            };
                            let value = crate::provider::claude::peer::inbound(&home);
                            Ok(json!({"account_id":format!("{provider}:{name}"),"provider":provider,"live_steering":value.as_deref()==Some(crate::provider::claude::peer::ACCEPT),
                                "setting":crate::provider::claude::peer::SETTING,"value":value,"previous":previous,"changed":changed,
                                "settings_file":crate::provider::claude::peer::settings_file(&home)}))
                        }
                        "account/bypass-warning" => {
                            let account = named(&state,params)?;
                            let provider = account.provider;
                            ops::require(ops::facts(provider).bypass_warning, "the bypass warning setting is Claude Code's skipDangerousModePermissionPrompt")?;
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let name = account.name.clone();
                            let home = crate::provider::account_home(&paths,account)?;
                            use crate::provider::claude::bypass;
                            let (changed, previous) = match params["skip"].as_bool() {
                                Some(skip) => {
                                    let t = bypass::set(&home, skip)?;
                                    if t.changed { store.save(&state,now(),if skip {"account.bypass_warning_skipped"} else {"account.bypass_warning_shown"},&name)?; }
                                    (t.changed, t.previous)
                                }
                                None => (false, bypass::skip(&home)),
                            };
                            let value = bypass::skip(&home);
                            Ok(json!({"account_id":format!("{provider}:{name}"),"provider":provider,"bypass_warning":if value==Some(true) {"skip"} else {"show"},
                                "setting":bypass::SETTING,"value":value,"previous":previous,"changed":changed,
                                "settings_file":crate::provider::claude::peer::settings_file(&home),"applies_to":"sessions_started_after_the_change"}))
                        }
                        "account/reorder" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            let account = named(&state,params)?;
                            let provider = account.provider; let key = account.key();
                            let position = params["position"].as_u64().context("INVALID_ARGUMENT: position must be a non-negative integer")? as usize;
                            let total = state.accounts.len();
                            ensure!(position < total, "INVALID_POSITION: position must be between 0 and {}", total - 1);
                            let changed = state.reorder(&key, position);
                            if changed { store.save(&state,now(),"account.reordered",name)?; }
                            let order: Vec<String> = state.ordered().iter().map(|a| format!("{}:{}", a.provider, a.name)).collect();
                            Ok(json!({"account_id":format!("{provider}:{name}"),"provider":provider,"position":position,"changed":changed,"order":order}))
                        }
                        "account/refresh" => {
                            let account = named(&state,params)?;
                            let provider = account.provider;
                            ops::require(crate::provider::native::is_native(provider), "Codex authentication is observed by the supervisor's own account poll")?;
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let name = account.name.clone();
                            let home = crate::provider::account_home(&paths,account)?;
                            let (problem, mode) = crate::provider::native::probe(&paths,&name,provider,&config,&home).await;
                            let key = crate::model::account_key(provider,&name);
                            last_poll.insert(key.clone(), now());
                            let a = state.accounts.get_mut(&key).context("unknown account")?;
                            let plan_changed = ops::facts(provider).auth_plan && problem.is_none()
                                && crate::plan::from_claude_auth(&mode, now()).is_some_and(|p| record_plan(&mut a.plan, &p));
                            if a.auth_required != problem.is_some() || a.auth_detail != problem || plan_changed {
                                a.auth_required = problem.is_some(); a.auth_detail = problem.clone();
                                store.save(&state,now(),"account_observed",&name)?;
                            }
                            Ok(json!({"account_id":format!("{provider}:{name}"),"provider":provider,"auth_required":problem.is_some(),"auth_detail":problem,"auth_mode":mode,"observed_at":now()}))
                        }
                        "account/reset-credit" => {
                            // Contract 1.53.0: spend one stored rate-limit reset credit. Journaled
                            // before the request; the request id is the idempotency key; never
                            // retried here, and never with a new key.
                            let account = codex_named(&state,params)?;
                            let (name, key) = (account.name.clone(), account.key());
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            ensure!(!account.auth_required,"AUTH_REQUIRED: log in to codex:{name} before redeeming a reset credit");
                            let request_id = params["request_id"].as_str().context("INVALID_REQUEST_ID: request_id is required")?.to_owned();
                            crate::reset_credit::validate_request_id(&request_id)?;
                            let credit_id = params["credit_id"].as_str().map(str::to_owned);
                            let _lock = crate::accounts::operation_lock(&paths,ProviderId::Codex,&name)?;
                            if let Some(reason) = monitoring_pause_reason(&paths,&name) {
                                bail!("ACCOUNT_BUSY: a Codex terminal holds codex:{name} without a shared app-server ({reason}); nothing was sent");
                            }
                            let account_id = format!("codex:{name}");
                            let mut record = crate::reset_credit::admit(&paths,ProviderId::Codex,&account_id,credit_id.as_deref(),&request_id,&version)?;
                            if let Some(helper) = helpers.get(&name) && !helper.is_running().await { helpers.remove(&name); }
                            if !helpers.contains_key(&name) {
                                // Adopts the account's shared app-server when it runs (Rpc::spawn).
                                match Rpc::spawn(&config.codex_binary,&paths.account(&name)?,&paths.root).await {
                                    Ok(rpc) => {helpers.insert(name.clone(),rpc);}
                                    Err(e) => {
                                        crate::reset_credit::finish(&paths,&mut record,"failed",None,"the account app-server could not start; nothing was sent")?;
                                        bail!("RESET_CREDIT_FAILED: the codex:{name} app-server could not start ({e}); nothing was sent");
                                    }
                                }
                            }
                            let rpc = helpers[&name].clone();
                            let sent = rpc.request_timeout(crate::reset_credit::METHOD,json!({"creditId":credit_id,"idempotencyKey":request_id}),Duration::from_secs(30)).await
                                .and_then(|v| crate::reset_credit::parse_outcome(&v));
                            let outcome = match sent {
                                Ok(outcome) => outcome,
                                Err(e) => {
                                    let (phase, message) = crate::reset_credit::failure(&e,&version,&request_id);
                                    crate::reset_credit::finish(&paths,&mut record,phase,None,&message)?;
                                    bail!("{message}");
                                }
                            };
                            crate::reset_credit::finish(&paths,&mut record,"complete",Some(&outcome),&format!("app-server answered {outcome}"))?;
                            let mut refreshed = false;
                            if matches!(outcome.as_str(),"reset"|"alreadyRedeemed") {
                                // Update the cached usage now, so usage, list and state show the reset.
                                if let Ok(usage) = rpc.request("account/rateLimits/read",json!({})).await.and_then(|v| parse_usage(&v,now())) {
                                    if let Some(a) = state.accounts.get_mut(&key) {
                                        // Contract 1.65.0: a reset starts a new cycle of every window.
                                        if outcome == "reset" { a.observe_redeemed(usage) } else { a.observe(usage) }
                                        refreshed = true;
                                    }
                                    last_poll.insert(name.clone(),now());
                                }
                            }
                            store.save(&state,now(),"account.reset_credit",&name)?;
                            let after = if refreshed {
                                state.accounts.get(&key).map(|a| account_usage(&paths,a,&config,now()).0).unwrap_or(Value::Null)
                            } else { Value::Null };
                            Ok(json!({"account_id":account_id,"request_id":request_id,"outcome":outcome,"rate_limits_after":after,
                                "attempts":record.attempts,"operation":crate::reset_credit::public(&record)}))
                        }
                        "account/rename" => {
                            let account = named(&state,params)?;
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let (name, provider) = (account.name.clone(), account.provider);
                            if let Some(expected) = params["expected_generation"].as_str() {
                                ensure!(crate::accounts::account_generation(account) == expected, "STALE_TARGET: the account changed since it was read; refresh the account list");
                            }
                            let next = params["display_name"].as_str().map(str::trim).filter(|d| !d.is_empty() && *d != name).map(str::to_owned);
                            if let Some(display) = &next {
                                validate_display_name(display)?;
                                if let Some(owner) = state.accounts.values().find(|a| a.provider == provider && a.name != name && (a.name == *display || a.display_name.as_deref() == Some(display))) {
                                    bail!("ACCOUNT_DISPLAY_NAME_EXISTS: '{display}' is already used by {}:{}; names and display names are unique per provider", owner.provider, owner.name);
                                }
                            }
                            let a = state.account_mut(provider,&name).context("unknown account")?;
                            let changed = a.display_name != next;
                            if changed { a.display_name = next.clone(); store.save(&state,now(),"account.renamed",&format!("{provider}:{name}"))?; }
                            let a = state.account(provider,&name).context("unknown account")?;
                            Ok(json!({"account_id":format!("{provider}:{name}"),"name":name,"folder_name":name,"display_name":next,"name_mismatch":a.display_name.is_some(),
                                "generation":crate::accounts::account_generation(a),"changed":changed}))
                        }
                        "account/managed-home" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            let account = named(&state,params)?;
                            ops::require(ops::facts(account.provider).adopts_config_dir, format_args!("{} accounts do not support managed-home", account.provider))?;
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let adopted = account.config_home.clone();
                            let previous = crate::provider::claude::check_managed_home(&paths,&config,name,adopted.as_deref()).await?;
                            let home = crate::provider::claude::default_home(&paths,name)?;
                            let Some(previous) = previous else {
                                return Ok(json!({"account_id":format!("claude:{name}"),"provider":"claude","home":home,"relocated":false,"already_managed":true,"note":"ALREADY_MANAGED"}));
                            };
                            state.account_mut(ProviderId::Claude,name).unwrap().config_home = None;
                            store.save(&state,now(),"account.claude_home_relocated",name)?;
                            Ok(json!({"account_id":format!("claude:{name}"),"provider":"claude","home":home,"previous_home":previous,"relocated":true,"already_managed":false}))
                        }
                        "account/enable" | "account/disable" => {
                            let key = key_of(&state,params)?; let name = key.as_str();
                            let account=state.accounts.get_mut(name).context("unknown account")?;ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");account.enabled = method == "account/enable";
                            store.save(&state,now(),"account_enabled_changed",name)?; Ok(json!({"saved":true}))
                        }
                        "account/lease" => {
                            let name = params["name"].as_str().context("missing account name")?; codex_named(&state,params)?;
                            ensure!(state.accounts.contains_key(name), "unknown account");
                            ensure!(state.accounts[name].deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            ensure!(!pending.contains(name), "account poll in progress; retry shortly");
                            let lease = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW).open(paths.account(name)?.join(".switcher-lease"))?;
                            ensure!(lease.try_lock_exclusive().is_err(), "account lease is not held by launcher");
                            if let Some(helper) = helpers.get(name) { helper.stop_idle().await?; }
                            helpers.remove(name);
                            Ok(json!({"leased":true}))
                        }
                        "account/prepare" | "account/restart-server" => {
                            let name = params["name"].as_str().context("missing account name")?; codex_named(&state,params)?;
                            let account = state.accounts.get(name).context("unknown account")?;
                            ensure!(account.enabled && !account.auth_required, "account needs login or is disabled");
                            ensure!(!pending.contains(name), "account poll in progress; retry shortly");
                            let home = paths.account(name)?;
                            let lease = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(home.join(".switcher-lease"))?;
                            let restarting = method == "account/restart-server";
                            let shared_ready=crate::control::Control::connect(&home).await.is_ok();
                            // Adopting an existing server creates no credential writer,
                            // even when an older handoff worker still holds its TUI lease.
                            if restarting || !shared_ready { lease.try_lock_exclusive().context("a legacy direct Codex launcher still owns this account; wait for it to exit")?; }
                            if restarting {
                                let mut control=crate::control::Control::connect(&home).await?;
                                ensure!(control.loaded().await?.is_empty(), "account backend is still serving conversations; source retained for recovery");
                                ensure!(!crate::external::host_instances(&paths)?.iter().any(|i|crate::layout::same_home(&i.home,&home)), "another Codex terminal is using the account backend");
                            }
                            if let Some(helper) = helpers.remove(name) { helper.stop_idle().await?; }
                            if let Err(e) = crate::provider::codex::pin_daemon_release(&home) { eprintln!("codex daemon release not pinned for {name}: {e:#}"); }
                            if restarting || !shared_ready {
                                let result = tokio::time::timeout(Duration::from_secs(15), tokio::process::Command::new(&config.codex_binary)
                                    .args(["app-server","daemon",if restarting {"restart"} else {"start"}])
                                    .env("CODEX_HOME", &home)
                                    .current_dir(&home)
                                    .env_remove("OPENAI_API_KEY").env_remove("CODEX_API_KEY").env_remove("CODEX_ACCESS_TOKEN")
                                    .output()).await.context("Codex server startup timed out")??;
                                ensure!(result.status.success(), "Codex account server could not start");
                            }
                            crate::control::Control::connect(&home).await.context("Codex started without a reachable control socket")?;
                            let rpc = Rpc::spawn(&config.codex_binary,&home,&paths.root).await?;
                            helpers.insert(name.into(),rpc);
                            Ok(json!({"socket":home.join("app-server-control/app-server-control.sock")}))
                        }
                        // Contract 1.64.0: delivery workers run as the supervisor's children.
                        "message/worker" => crate::messaging::claude_steer::supervised_start(&paths,params),
                        _ => bail!("unknown control method"),
                    }
                }.await;
                if result.is_ok() && !state.normalize_order().is_empty() {
                    let _ = store.save(&state, now(), "account.order_assigned", "");
                }
                let response = match result {Ok(result) => json!({"id":request.value["id"],"result":result}),Err(error) => json!({"id":request.value["id"],"error":{"code":if method.starts_with("session/") {"NATIVE_HANDOFF_UNVERIFIED"} else {"REQUEST_FAILED"},"message":error.to_string()}})};
                let _ = request.reply.send(response);
                if stopping {
                    // Let the socket task write the acknowledgement before this
                    // process tears down its runtime (especially with zero helpers).
                    let _ = tokio::time::timeout(Duration::from_secs(2),request.delivered).await;
                    break;
                }
            }
            Some(result) = polls.recv() => {
                pending.remove(&result.name);
                apply_poll(&mut state,&mut store,result)?;
            }
            Some(result) = claude_polls.recv() => {
                pending.remove(&result.name);
                if let Some(a) = state.accounts.get_mut(&result.name)
                    && crate::provider::native::is_native(a.provider)
                {
                    let required = result.problem.is_some();
                    let plan_changed = result.plan.as_ref().is_some_and(|p| record_plan(&mut a.plan, p));
                    if a.auth_required != required || a.auth_detail != result.problem || plan_changed {
                        a.auth_required = required;
                        a.auth_detail = result.problem;
                        store.save(&state,now(),"account_observed",&result.name)?;
                    }
                }
            }
            Some((at, counts)) = history_rx.recv() => {
                // History is observational; a storage failure must not stop monitoring/accounts.
                if let Err(error) = history.observe(&mut store,&state.accounts,at,config.stale_seconds,counts) {
                    eprintln!("usage history: {error:#}");
                }
            }
            _ = interval.tick() => {
                for a in state.accounts.values() {
                    let akey = a.key();
                    if !a.enabled || pending.contains(&akey) { continue; }
                    if crate::provider::native::is_native(a.provider) {
                        // Auth status only: Claude usage comes from its statusline and
                        // Gemini has none; never a model request or a Codex worker.
                        let cadence = if a.auth_required {CLAUDE_AUTH_REQUIRED_POLL_SECONDS} else {config.idle_poll_seconds as i64};
                        if now() - last_poll.get(&akey).copied().unwrap_or(0) < cadence {continue;}
                        last_poll.insert(akey.clone(),now());
                        let Ok(home) = crate::provider::account_home(&paths,a) else {continue};
                        pending.insert(akey.clone());
                        let (name, key, config, tx, provider, owned) = (a.name.clone(), akey.clone(), config.clone(), claude_tx.clone(), a.provider, paths.clone());
                        tokio::spawn(async move {
                            let (problem, mode) = crate::provider::native::probe(&owned,&name,provider,&config,&home).await;
                            let plan = (ops::facts(provider).auth_plan && problem.is_none()).then(|| crate::plan::from_claude_auth(&mode, now())).flatten();
                            let _ = tx.send(ClaudePoll{name:key,problem,plan}).await;
                        });
                        continue;
                    }
                    // Contract 1.62.1: the two safety skips (a terminal may be opening on the account;
                    // a session holds its lease with no shared app-server) are one decision, and
                    // the meter says why it is not being read.
                    if let Some(code) = monitoring_pause_reason(&paths,&a.name) {
                        set_meter_note(&a.name, Some(pause_sentence(code).to_owned()));
                        continue;
                    }
                    let near = pressure(a,&config) >= 0.8;
                    let cadence = if a.auth_required {5} else if near {config.near_limit_poll_seconds} else {config.idle_poll_seconds};
                    let reset_due = [&a.usage.short,&a.usage.weekly].iter().filter_map(|w|w.as_ref()).any(|w|w.resets_at <= now() && last_poll.get(&a.name).copied().unwrap_or(0) < w.resets_at);
                    if !reset_due && now() - last_poll.get(&a.name).copied().unwrap_or(0) < cadence as i64 {continue;}
                    last_poll.insert(a.name.clone(),now());
                    if let Some(helper) = helpers.get(&a.name) {
                        if !helper.is_running().await {helpers.remove(&a.name);}
                    }
                    if !helpers.contains_key(&a.name) {
                        match Rpc::spawn(&config.codex_binary,&paths.account(&a.name)?,&paths.root).await {
                            Ok(rpc) => {helpers.insert(a.name.clone(),rpc);}, Err(e) => { set_meter_note(&a.name, Some(format!("the app-server could not be reached or started: {e}"))); continue; }
                        }
                    }
                    pending.insert(a.name.clone());
                    let rpc = helpers[&a.name].clone(); let name = a.name.clone(); let home = paths.account(&name)?; let tx = poll_tx.clone();
                    tokio::spawn(async move { let result = poll(name,rpc,home).await; let _ = tx.send(result).await; });
                }
            }
        }
    }
    version_probe.abort();
    auto_recover_task.abort();
    compaction_task.abort();
    model_task.abort();
    message_worker_task.abort();
    meter_task.abort();
    events_task.abort();
    snapshot_task.abort();
    history_task.abort();
    accept_task.abort();
    for (_, helper) in helpers {
        helper.stop_idle().await.ok();
    }
    std::fs::remove_file(socket).ok();
    Ok(())
}

pub async fn call(paths: &Paths, method: &str, params: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(paths.control())
        .await
        .context("supervisor is not running; run `sessionsation daemon start`")?;
    let mut bytes = serde_json::to_vec(&json!({"id":1,"method":method,"params":params}))?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let value = tokio::time::timeout(
        Duration::from_secs(70),
        read_frame(&mut BufReader::new(stream)),
    )
    .await??
    .context("empty supervisor response")?;
    if let Some(error) = value.get("error") {
        bail!(
            "{}: {}",
            error["code"].as_str().unwrap_or("ERROR"),
            error["message"].as_str().unwrap_or("request failed")
        );
    }
    Ok(value["result"].clone())
}

pub async fn ensure_started(paths: &Paths) -> Result<()> {
    if call(paths, "status", json!({})).await.is_ok() {
        return Ok(());
    }
    let executable = crate::self_executable::path()?;
    let log_path = paths.root.join(crate::layout::DAEMON_LOG);
    private_dir(log_path.parent().context("log parent")?)?;
    use std::os::unix::fs::OpenOptionsExt;
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log_path)?;
    let mut child = tokio::process::Command::new(executable);
    // SAFETY: close_inherited_fds only makes raw async-signal-safe syscalls.
    unsafe { child.pre_exec(crate::background::close_inherited_fds) };
    child
        .arg("--data-dir")
        .arg(&paths.root)
        .args(["daemon", "serve"])
        // Contract 1.62.0: a supervisor started from inside a session never hands that
        // session's id to the app-servers and workers it starts.
        .env_remove(crate::provider::SESSION_ENV)
        .current_dir(&paths.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .process_group(0);
    let mut child = child.spawn()?;
    for _ in 0..100 {
        if call(paths, "status", json!({})).await.is_ok() {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            // A concurrent launcher can win the daemon lock.
            if call(paths, "status", json!({})).await.is_ok() {
                return Ok(());
            }
            bail!("supervisor did not start; inspect daemon.log");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("supervisor startup timed out")
}

fn pressure(account: &Account, config: &Config) -> f64 {
    if !account.usage.windows_complete() {
        return f64::INFINITY;
    }
    [
        (&account.usage.short, config.short_ceiling),
        (&account.usage.weekly, config.weekly_ceiling),
    ]
    .into_iter()
    .filter_map(|(w, ceiling)| w.as_ref().map(|w| w.used_percent / ceiling))
    .fold(0., f64::max)
}
fn eligible(account: &Account, config: &Config, now: i64) -> bool {
    let u = &account.usage;
    if !account.enabled
        || account.auth_required
        || !u.observed_fresh(now, config.stale_seconds)
        || u.spend_control_reached == Some(true)
    {
        return false;
    }
    // A reached plan allowance can still be served from purchased credits.
    if (u.exhausted || u.ordinary_usage_allowed == Some(false))
        && u.credits.as_ref().is_some_and(|c| c.available())
        && matches!(u.reached_type.as_deref(), None | Some("rate_limit_reached"))
    {
        return true;
    }
    let below = |w: &Option<crate::model::Window>, ceiling: f64| {
        w.as_ref()
            .is_none_or(|w| w.used_percent < ceiling - config.drain_margin)
    };
    !u.exhausted
        && u.ordinary_usage_allowed != Some(false)
        && u.fresh(now, config.stale_seconds)
        && below(&u.short, config.short_ceiling)
        && below(&u.weekly, config.weekly_ceiling)
}
async fn codex_version(binary: &str) -> Result<String> {
    let run = tokio::process::Command::new(binary)
        .arg("--version")
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), run).await??;
    ensure!(out.status.success(), "Codex version command failed");
    Ok(String::from_utf8(out.stdout)?.trim().into())
}

#[cfg(test)]
mod tests;
