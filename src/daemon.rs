use crate::{
    compatibility,
    config::Config,
    model::{Account, Credits, State, Usage, Window},
    paths::{Paths, private_dir},
    provider::ProviderId,
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
}

pub fn parse_usage(value: &Value, at: i64) -> Result<Usage> {
    let limits = if value["rateLimitsByLimitId"]["codex"].is_object() {
        &value["rateLimitsByLimitId"]["codex"]
    } else {
        &value["rateLimits"]
    };
    ensure!(limits.is_object(), "missing usage limits");
    if let Some(id) = limits["limitId"].as_str() {
        ensure!(id == "codex", "unexpected usage bucket");
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
    ensure!(
        account.provider == ProviderId::Codex,
        "UNSUPPORTED_PROVIDER_OPERATION: {} accounts do not support this operation",
        account.provider
    );
    Ok(account)
}
/// Display names follow account-name characters, up to 30 bytes.
pub fn validate_display_name(display: &str) -> Result<()> {
    ensure!(
        display.len() <= crate::paths::NEW_ACCOUNT_NAME_MAX
            && crate::paths::validate_name(display).is_ok(),
        "INVALID_DISPLAY_NAME: use 1-30 ASCII letters, digits, hyphens or underscores"
    );
    Ok(())
}
/// While a Claude account awaits login, re-probe `auth status` this often so a
/// completed interactive login is reflected within seconds.
pub const CLAUDE_AUTH_REQUIRED_POLL_SECONDS: i64 = 3;
struct ClaudePoll {
    name: String,
    problem: Option<String>,
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

/// Provider usage in the shared account shape plus `source`/`semantics`.
/// Codex values are the supervisor's snapshot; Claude values are projected from
/// the statusline recorder; Gemini reports no usage (state `unsupported`). Returns (usage, usage_fresh, usage_state).
fn account_usage(paths: &Paths, a: &Account, config: &Config, at: i64) -> (Value, bool, String) {
    match a.provider {
        ProviderId::Codex => {
            let fresh = a.usage.fresh(at, config.stale_seconds);
            let mut usage = json!(a.usage);
            if a.usage.observed_at == 0 {
                usage["observed_at"] = Value::Null;
            }
            usage["source"] = json!(crate::provider::codex::USAGE_SOURCE);
            usage["semantics"] = json!(crate::provider::codex::USAGE_SEMANTICS);
            let state = if fresh {
                "fresh"
            } else if a.usage.observed_at == 0 {
                "missing"
            } else {
                "stale"
            };
            (usage, fresh, state.into())
        }
        ProviderId::Gemini => (
            json!({"short":null,"weekly":null,"observed_at":null,"exhausted":false,"state":"unsupported",
                "source":crate::provider::gemini::USAGE_SOURCE,"semantics":crate::provider::gemini::USAGE_SEMANTICS}),
            false,
            "unsupported".into(),
        ),
        ProviderId::Antigravity => (
            json!({"short":null,"weekly":null,"observed_at":null,"exhausted":false,"state":"unsupported",
                "source":crate::provider::antigravity::USAGE_SOURCE,"semantics":crate::provider::antigravity::USAGE_SEMANTICS}),
            false,
            "unsupported".into(),
        ),
        ProviderId::Claude => match crate::provider::account_home(paths, a) {
            Ok(home) => {
                let (usage, fresh, state) = crate::provider::claude::project_usage(&home, at);
                (usage, fresh, state.into())
            }
            Err(_) => (
                json!({"short":null,"weekly":null,"observed_at":null,"exhausted":false,"state":"error",
                    "source":crate::provider::claude::USAGE_SOURCE,"semantics":crate::provider::claude::USAGE_SEMANTICS}),
                false,
                "error".into(),
            ),
        },
    }
}

pub fn status(paths: &Paths, state: &State, config: &Config, version: &str) -> Value {
    let at = now();
    let accounts: Vec<_> = state.ordered().into_iter().map(|a| {
        let (usage, fresh, usage_state) = account_usage(paths, a, config, at);
        json!({
        "name":a.name,"folder_name":a.name,"name_mismatch":a.display_name.is_some(),"generation":crate::account_rename::account_generation(a),
        "order":a.order,"display_name":a.display_name,"provider":a.provider,"config_home":a.config_home,
        "enabled":a.enabled,"auth_required":a.auth_required,"auth_detail":a.auth_detail,"deletion_id":a.deletion_id,
        "switch_thresholds":a.switch_thresholds,
        "usage":usage,"usage_fresh":fresh,"usage_state":usage_state,
        "credit_fresh":a.provider == ProviderId::Codex && a.usage.credits.is_some() && a.usage.observed_fresh(at,config.stale_seconds),
        "monitoring_pause_reason":(a.provider == ProviderId::Codex).then(||monitoring_pause_reason(paths,&a.name)).flatten(),
        "eligible":a.provider == ProviderId::Codex && crate::scheduler::eligible(a,config,at),
        "assigned_sessions":state.sessions.values().filter(|s|a.provider == ProviderId::Codex && s.account.as_ref()==Some(&a.name)).count()
    })}).collect();
    let sessions: Vec<_> = state.sessions.values().map(|s|json!({"id":s.id,"thread_id":s.root_thread,"cwd":s.cwd,"account":s.account,"phase":s.phase,"reason":s.reason,"tmux_pane":s.tmux_pane,"manual_pause":s.manual_pause,"resume_work":s.resume_work})).collect();
    json!({"schema_version":1,"daemon":{"running":true,"package_version":env!("CARGO_PKG_VERSION"),"api_version":1,"lifecycle_reservations":["message-recoveries","space-moves"]},"codex_version":version,
        "capabilities":{"account_deletion":true,"account_monitoring":true,"scheduler_simulation":true,"manual_exit_handoff":true,"manual_moves":true,"explicit_account_launch":true,"session_inventory":true,"session_open":true,"archive_recovery":true,"project_relocation":true,"session_restart":true,"session_termination":true,"session_pause":true,"job_retry":true,"verified_paginated_moves":true,"managed_launch":false,"experimental_auto_start":true,"experimental_auto_switching":true,"automatic_switching":false,"conversation_import":false},
        "readiness":{"ready":false,"code":"NATIVE_HANDOFF_UNVERIFIED","detail":"Native handoff compatibility gates have not passed. See docs/COMPATIBILITY.md."},
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
    let usage = if identity.is_some() {
        match rpc.request("account/rateLimits/read", json!({})).await {
            Ok(v) => parse_usage(&v, now()).ok(),
            Err(_) => None,
        }
    } else {
        None
    };
    Poll {
        name,
        identity,
        usage,
        unauthenticated,
    }
}

pub async fn serve(paths: Paths) -> Result<()> {
    std::env::set_current_dir(&paths.root)?;
    // A crash between create and rename of an atomic write leaves a `.tmp*` file.
    crate::paths::sweep_stale_temps(&paths.root, crate::paths::STALE_TEMP_AGE);
    // Must precede removing the stale socket and opening the database.
    let mut store = Store::open(&paths)?;
    let mut state = store.load()?;
    crate::automatic::reconcile(&mut state);
    if !state.normalize_order().is_empty() {
        store.save(&state, now(), "account.order_migrated", "")?;
    }
    crate::autostart::reconcile(&mut state);
    crate::account_rename::reconcile_all(&paths, &mut state, &mut store);
    store.save(&state, now(), "automatic_reconciled", "")?;
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
        if account.provider != ProviderId::Codex || account.deletion_id.is_some() {
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
    let version = compatibility::version(&config.codex_binary)
        .await
        .unwrap_or_else(|_| "unavailable".into());
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
    let automatic_task = tokio::spawn(crate::automatic::monitor(paths.clone()));
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
    let mut stopping = false;
    loop {
        tokio::select! {
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
                        method if method.starts_with("automatic/") => {
                            let value = crate::automatic::request(&paths,&mut state,&config,method,params)?;
                            if method != "automatic/get" { store.save(&state,now(),method,params["thread"].as_str().unwrap_or(""))?; }
                            Ok(value)
                        }
                        method if method.starts_with("auto-start/") => {
                            let value = crate::autostart::request(&mut state,&config,method,params)?;
                            if method != "auto-start/get" { store.save(&state,now(),method,params["thread"].as_str().unwrap_or(""))?; }
                            Ok(value)
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
                        "config/get" => Ok(serde_json::to_value(&config)?),
                        "config/set" => {
                            let new: Config = serde_json::from_value(params.clone())?; new.validate()?;
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
                            ensure!(account.config_home==record.external_home,"ACCOUNT_DELETION_IDENTITY_MISMATCH");
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
                            if provider == ProviderId::Gemini {
                                ensure!(config_dir.is_none(), "INVALID_ARGUMENT: --config-dir is only supported for claude accounts");
                                let (home, existing) = crate::provider::gemini::create_home(&paths,name)?;
                                let auth = crate::provider::gemini::auth_report(&home);
                                let mut account = Account::with_provider(ProviderId::Gemini,name.into(),String::new());
                                account.auth_required = !auth["problem"].is_null();
                                account.auth_detail = auth["problem"].as_str().map(str::to_owned);
                                state.insert_account(account); last_poll.remove(&key);
                                store.save(&state,now(),"account_added",name)?;
                                return Ok(json!({"name":name,"provider":"gemini","account_id":format!("gemini:{name}"),"home":home,"adopted":false,"existing_home":existing,"auth":auth,
                                    "login":{"action":"exec","interactive":true,"command":["sessionsation","account","login",format!("gemini:{name}")]}}));
                            }
                            if provider == ProviderId::Antigravity {
                                ensure!(config_dir.is_none(), "INVALID_ARGUMENT: --config-dir is only supported for claude accounts");
                                let (home, existing) = crate::provider::antigravity::create_home(&paths,name)?;
                                let auth = crate::provider::antigravity::auth_report(&home);
                                let mut account = Account::with_provider(ProviderId::Antigravity,name.into(),String::new());
                                account.auth_required = !auth["problem"].is_null();
                                account.auth_detail = auth["problem"].as_str().map(str::to_owned);
                                state.insert_account(account); last_poll.remove(&key);
                                store.save(&state,now(),"account_added",name)?;
                                return Ok(json!({"name":name,"provider":"antigravity","account_id":format!("antigravity:{name}"),"home":home,"adopted":false,"existing_home":existing,"auth":auth,
                                    "login":{"action":"exec","interactive":true,"command":["sessionsation","account","login",format!("antigravity:{name}")]}}));
                            }
                            if provider == ProviderId::Claude {
                                let adopted = config_dir.map(|dir| crate::provider::claude::validate_adopt(&paths,&state,&dir)).transpose()?;
                                // The managed home is the normal layout; an operator-populated one is registered as is.
                                let (home, existing) = match &adopted { Some(dir) => (dir.clone(), true), None => crate::provider::claude::create_home(&paths,name)? };
                                if adopted.is_none() && !existing { if let Err(e) = crate::provider::claude::bypass::default_new(&home) { eprintln!("bypass-warning default not set: {e}"); } }
                                if adopted.is_none() && !existing { if let Err(e) = crate::provider::claude::statusline_seed::default_new(&home,&config.claude_default_statusline_command) { eprintln!("statusLine default not set: {e}"); } }
                                let auth = crate::provider::claude::auth_report(&config,&home).await;
                                let mut account = Account::with_provider(ProviderId::Claude,name.into(),String::new());
                                account.config_home = adopted; account.auth_required = !auth["problem"].is_null();
                                account.auth_detail = auth["problem"].as_str().map(str::to_owned);
                                state.insert_account(account); last_poll.remove(&key);
                                store.save(&state,now(),"account_added",name)?;
                                return Ok(json!({"name":name,"provider":"claude","account_id":format!("claude:{name}"),"home":home,"adopted":params["config_dir"].is_string(),"existing_home":existing,"auth":auth,
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
                            ensure!(state.accounts[name].provider == ProviderId::Codex, "UNSUPPORTED_PROVIDER_OPERATION: Claude and Gemini login is interactive; run `sessionsation account login <provider>:{name}` in a terminal");
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
                            ensure!(provider != ProviderId::Antigravity, "{}", crate::provider::antigravity::NO_LOGOUT);
                            crate::logout::ensure_idle(&paths,provider,name,&home).await?;
                            match provider {
                                ProviderId::Codex => crate::logout::codex(&config,&home,helpers.get(&key)).await?,
                                ProviderId::Claude => crate::logout::claude(&config,&home).await?,
                                ProviderId::Gemini => crate::logout::gemini(&home)?,
                                ProviderId::Antigravity => bail!("{}", crate::provider::antigravity::NO_LOGOUT),
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
                            ensure!(matches!(provider, ProviderId::Claude | ProviderId::Gemini), "UNSUPPORTED_PROVIDER_OPERATION: live steering uses Claude Code peer messaging or Gemini CLI model steering");
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let name = account.name.clone();
                            let home = crate::provider::account_home(&paths,account)?;
                            if provider == ProviderId::Gemini {
                                // Gemini: experimental.modelSteering, read by each session at startup.
                                use crate::provider::gemini::steering;
                                let (changed, previous) = match params["enable"].as_bool() {
                                    Some(enable) => {
                                        let t = steering::set(&home, enable)?;
                                        if t.changed { store.save(&state,now(),if enable {"account.live_steering_enabled"} else {"account.live_steering_disabled"},&name)?; }
                                        (t.changed, t.previous)
                                    }
                                    None => (false, steering::configured(&home).map(Value::Bool)),
                                };
                                let value = steering::configured(&home);
                                let running = crate::provider::gemini::live_sessions(&home).map(|l| l.len()).unwrap_or(0);
                                return Ok(json!({"account_id":format!("{provider}:{name}"),"provider":provider,"live_steering":value==Some(true),
                                    "setting":steering::SETTING,"value":value,"previous":previous,"changed":changed,
                                    "settings_file":steering::settings_file(&home),"applies_to":"sessions_started_after_the_change",
                                    "restart_required":changed && running > 0,"running_sessions":running}));
                            }
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
                            ensure!(provider == ProviderId::Claude, "UNSUPPORTED_PROVIDER_OPERATION: the bypass warning setting is Claude Code's skipDangerousModePermissionPrompt");
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
                            ensure!(crate::provider::native::is_native(provider), "UNSUPPORTED_PROVIDER_OPERATION: Codex authentication is observed by the supervisor's own account poll");
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let name = account.name.clone();
                            let home = crate::provider::account_home(&paths,account)?;
                            let (problem, mode) = crate::provider::native::probe(&paths,&name,provider,&config,&home).await;
                            let key = crate::model::account_key(provider,&name);
                            last_poll.insert(key.clone(), now());
                            let a = state.accounts.get_mut(&key).context("unknown account")?;
                            if a.auth_required != problem.is_some() || a.auth_detail != problem {
                                a.auth_required = problem.is_some(); a.auth_detail = problem.clone();
                                store.save(&state,now(),"account_observed",&name)?;
                            }
                            Ok(json!({"account_id":format!("{provider}:{name}"),"provider":provider,"auth_required":problem.is_some(),"auth_detail":problem,"auth_mode":mode,"observed_at":now()}))
                        }
                        "account/rename" => {
                            let account = named(&state,params)?;
                            ensure!(account.deletion_id.is_none(),"ACCOUNT_DELETION_IN_PROGRESS");
                            let (name, provider) = (account.name.clone(), account.provider);
                            if let Some(expected) = params["expected_generation"].as_str() {
                                ensure!(crate::account_rename::account_generation(account) == expected, "STALE_TARGET: the account changed since it was read; refresh the account list");
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
                                "generation":crate::account_rename::account_generation(a),"changed":changed}))
                        }
                        "account/rename-backend" => {
                            let account = named(&state,params)?.clone();
                            let (provider, key) = (account.provider, account.key());
                            let request_id = params["request_id"].as_str().context("INVALID_ARGUMENT: request_id is required")?;
                            uuid::Uuid::parse_str(request_id).context("INVALID_ARGUMENT: request_id must be a UUID")?;
                            if crate::account_rename::exists(&paths,request_id) {
                                // Never replayed: report the recorded operation.
                                let record = crate::account_rename::read(&paths,request_id)?;
                                return Ok(json!({"already_recorded":true,"operation":crate::account_rename::public(&record)}));
                            }
                            let expected = params["expected_generation"].as_str().context("INVALID_ARGUMENT: expected_generation is required")?;
                            // A native auth probe in flight is harmless (its result is dropped once the
                            // key is gone); a Codex helper still holds the home.
                            ensure!(provider != ProviderId::Codex || !pending.contains(&key), "ACCOUNT_POLL_IN_PROGRESS: account poll is in progress; retry shortly");
                            let plan = crate::account_rename::plan(&paths,&state,provider,&account.name,params["new_name"].as_str(),expected)?;
                            let _old_lock = crate::accounts::operation_lock(&paths,provider,&plan.old_name)?;
                            let _new_lock = crate::accounts::operation_lock(&paths,provider,&plan.new_name)?;
                            crate::account_rename::ensure_idle(&paths,&plan).await?;
                            if provider == ProviderId::Codex {
                                if let Some(helper) = helpers.remove(&key) { helper.stop_idle().await?; }
                                crate::account_rename::stop_codex_server(&config,&plan.old_home).await?;
                            }
                            last_poll.remove(&key);
                            let (previous_id, new_name) = (format!("{provider}:{}",plan.old_name), plan.new_name.clone());
                            let operation = crate::account_rename::execute(&paths,&mut state,&mut store,plan,request_id,&|_| Ok(()))?;
                            let a = state.account(provider,&new_name).context("unknown account")?;
                            Ok(json!({"account_id":format!("{provider}:{new_name}"),"previous_account_id":previous_id,"name":new_name,"folder_name":new_name,
                                "display_name":a.display_name,"name_mismatch":a.display_name.is_some(),"generation":crate::account_rename::account_generation(a),
                                "home":crate::layout::account_home(&paths.root,provider,&new_name),"operation":operation,"changed":true}))
                        }
                        "account/rename-backend/recover" => {
                            let id = params["request_id"].as_str().context("INVALID_ARGUMENT: request_id is required")?;
                            let operation = crate::account_rename::reconcile(&paths,&mut state,&mut store,id,&|_| Ok(()))?;
                            Ok(json!({"operation":operation}))
                        }
                        "account/managed-home" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            let account = named(&state,params)?;
                            ensure!(account.provider == ProviderId::Claude, "UNSUPPORTED_PROVIDER_OPERATION: {} accounts do not support managed-home", account.provider);
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
                                ensure!(!crate::external::host_instances(&paths)?.iter().any(|i|i.home==home), "another Codex terminal is using the account backend");
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
                        "session/run" | "session/import" | "session/pause" | "session/resume" => {
                            compatibility::require_native_handoff()?; unreachable!()
                        }
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
                let duplicate = result.identity.as_ref().is_some_and(|identity|state.accounts.values().any(|a|a.key() != result.name && &a.identity == identity));
                if let Some(a) = state.accounts.get_mut(&result.name) {
                    if duplicate || result.identity.as_ref().is_some_and(|id|!a.identity.is_empty() && id != &a.identity) {
                        a.auth_required = true; a.enabled = false;
                    } else if let Some(identity) = result.identity {
                        a.identity = identity; a.auth_required = false;
                        if let Some(usage) = result.usage {a.observe(usage);}
                    } else if result.unauthenticated { a.auth_required = true; }
                    store.save(&state,now(),"account_observed",&result.name)?;
                }
            }
            Some(result) = claude_polls.recv() => {
                pending.remove(&result.name);
                if let Some(a) = state.accounts.get_mut(&result.name)
                    && crate::provider::native::is_native(a.provider)
                {
                    let required = result.problem.is_some();
                    if a.auth_required != required || a.auth_detail != result.problem {
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
                            let problem = crate::provider::native::probe(&owned,&name,provider,&config,&home).await.0;
                            let _ = tx.send(ClaudePoll{name:key,problem}).await;
                        });
                        continue;
                    }
                    let shared = crate::control::available(&paths.account(&a.name)?);
                    if !shared && crate::external::destination_tui_may_be_active(&paths,&a.name) {continue;}
                    let lease_path = paths.account(&a.name)?.join(".switcher-lease");
                    if let Ok(lease) = OpenOptions::new().read(true).write(true).open(&lease_path) {
                        if lease.try_lock_exclusive().is_err() {
                            if !shared && !crate::external::inactive_source_only_lease(&paths, &a.name) { continue; }
                        } else {
                            let _ = FileExt::unlock(&lease);
                        }
                    }
                    let automatic = state.automatic_switching.values().any(|p| p.enabled && p.accounts.contains(&a.name))
                        || state.automatic_start.values().any(|p|p.enabled);
                    let threshold_near = a.usage.short.as_ref().is_some_and(|w| w.used_percent >= a.switch_thresholds.short_percent - 5.)
                        || a.usage.weekly.as_ref().is_some_and(|w| w.used_percent >= a.switch_thresholds.weekly_percent - 5.);
                    let near = crate::scheduler::pressure(a,&config) >= 0.8 || (automatic && threshold_near);
                    let cadence = if a.auth_required {5} else if near {config.near_limit_poll_seconds} else if automatic {config.active_poll_seconds} else {config.idle_poll_seconds};
                    let reset_due = [&a.usage.short,&a.usage.weekly].iter().filter_map(|w|w.as_ref()).any(|w|w.resets_at <= now() && last_poll.get(&a.name).copied().unwrap_or(0) < w.resets_at);
                    if !reset_due && now() - last_poll.get(&a.name).copied().unwrap_or(0) < cadence as i64 {continue;}
                    last_poll.insert(a.name.clone(),now());
                    if let Some(helper) = helpers.get(&a.name) {
                        if !helper.is_alive().await {helpers.remove(&a.name);}
                    }
                    if !helpers.contains_key(&a.name) {
                        match Rpc::spawn(&config.codex_binary,&paths.account(&a.name)?,&paths.root).await {
                            Ok(rpc) => {helpers.insert(a.name.clone(),rpc);}, Err(_) => continue,
                        }
                    }
                    pending.insert(a.name.clone());
                    let rpc = helpers[&a.name].clone(); let name = a.name.clone(); let home = paths.account(&name)?; let tx = poll_tx.clone();
                    tokio::spawn(async move { let result = poll(name,rpc,home).await; let _ = tx.send(result).await; });
                }
            }
        }
    }
    automatic_task.abort();
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
    let mut child = tokio::process::Command::new(executable)
        .arg("--data-dir")
        .arg(&paths.root)
        .args(["daemon", "serve"])
        .current_dir(&paths.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unobserved_usage_reports_null_observed_at_never_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let config = Config::default();
        let mut state = State::default();
        state
            .accounts
            .insert("cx".into(), Account::new("cx".into(), "fixture".into()));
        state
            .accounts
            .insert("real".into(), Account::new("real".into(), "fixture".into()));
        state.accounts.get_mut("real").unwrap().usage.observed_at = 1234;
        state.accounts.insert(
            "gm".into(),
            Account::with_provider(ProviderId::Gemini, "gm".into(), String::new()),
        );
        state.accounts.insert(
            "cl".into(),
            Account::with_provider(ProviderId::Claude, "cl".into(), String::new()),
        );
        let report = status(&paths, &state, &config, "fixture");
        for a in report["accounts"].as_array().unwrap() {
            let observed = &a["usage"]["observed_at"];
            match a["name"].as_str().unwrap() {
                "real" => assert_eq!(observed, 1234),
                name => assert!(observed.is_null(), "{name}: {observed}"),
            }
        }
    }

    #[test]
    fn locked_account_reports_paused_monitoring_without_claiming_fresh_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let home = paths.account("personal").unwrap();
        private_dir(&home).unwrap();
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(home.join(".switcher-lease"))
            .unwrap();
        lease.try_lock_exclusive().unwrap();
        let mut state = State::default();
        state.accounts.insert(
            "personal".into(),
            Account::new("personal".into(), "fixture".into()),
        );
        let report = status(&paths, &state, &Config::default(), "fixture");
        assert_eq!(
            report["accounts"][0]["monitoring_pause_reason"],
            "account_lease"
        );
        assert_eq!(report["accounts"][0]["usage_fresh"], false);
        FileExt::unlock(&lease).unwrap();
    }
}
