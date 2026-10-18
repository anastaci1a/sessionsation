use crate::{
    compatibility,
    config::Config,
    model::{Account, Credits, State, Usage, Window},
    paths::{Paths, private_dir, validate_name},
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

pub fn status(paths: &Paths, state: &State, config: &Config, version: &str) -> Value {
    let at = now();
    let accounts: Vec<_> = state.accounts.values().map(|a|json!({
        "name":a.name,"enabled":a.enabled,"auth_required":a.auth_required,
        "switch_thresholds":a.switch_thresholds,
        "usage":a.usage,"usage_fresh":a.usage.fresh(at,config.stale_seconds),
        "credit_fresh":a.usage.credits.is_some() && a.usage.observed_fresh(at,config.stale_seconds),
        "monitoring_pause_reason":monitoring_pause_reason(paths,&a.name),
        "eligible":crate::scheduler::eligible(a,config,at),
        "assigned_sessions":state.sessions.values().filter(|s|s.account.as_ref()==Some(&a.name)).count()
    })).collect();
    let sessions: Vec<_> = state.sessions.values().map(|s|json!({"id":s.id,"thread_id":s.root_thread,"cwd":s.cwd,"account":s.account,"phase":s.phase,"reason":s.reason,"tmux_pane":s.tmux_pane,"manual_pause":s.manual_pause,"resume_work":s.resume_work})).collect();
    json!({"schema_version":1,"daemon":{"running":true},"codex_version":version,
        "capabilities":{"account_monitoring":true,"scheduler_simulation":true,"manual_exit_handoff":true,"manual_moves":true,"explicit_account_launch":true,"session_inventory":true,"session_open":true,"archive_recovery":true,"project_relocation":true,"session_restart":true,"session_termination":true,"session_pause":true,"job_retry":true,"verified_paginated_moves":true,"managed_launch":false,"experimental_auto_start":true,"experimental_auto_switching":true,"automatic_switching":false,"conversation_import":false},
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
    // Must precede removing the stale socket and opening the database.
    let mut store = Store::open(&paths)?;
    let mut state = store.load()?;
    crate::automatic::reconcile(&mut state);
    crate::autostart::reconcile(&mut state);
    store.save(&state, now(), "automatic_reconciled", "")?;
    let mut config = load_config(&paths)?;
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
                            let name = params["name"].as_str().context("missing account name")?;
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
                            let name = params["name"].as_str().context("missing account name")?;
                            let account = state.accounts.get_mut(name).context("unknown account")?;
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
                        "account/add" => {
                            let name = params["name"].as_str().context("missing account name")?; validate_name(name)?;
                            ensure!(!state.accounts.contains_key(name), "account name already exists; use account login to reauthenticate");
                            let home = paths.account(name)?; private_dir(&home)?;
                            let rpc = Rpc::spawn(&config.codex_binary,&home,&paths.root).await?;
                            let login = rpc.request("account/login/start",json!({"type":"chatgptDeviceCode"})).await;
                            match login {
                                Ok(login) => {
                                    let mut account = Account::new(name.into(),String::new()); account.auth_required = true;
                                    state.accounts.insert(name.into(),account); helpers.insert(name.into(),rpc);
                                    store.save(&state,now(),"account_added",name)?;
                                    Ok(json!({"name":name,"login":login}))
                                },
                                Err(e) => {rpc.stop_idle().await.ok(); Err(e)}
                            }
                        }
                        "account/login" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            ensure!(state.accounts.contains_key(name), "unknown account");
                            ensure!(!pending.contains(name), "account poll is in progress; retry shortly");
                            if !helpers.contains_key(name) {let rpc = Rpc::spawn(&config.codex_binary,&paths.account(name)?,&paths.root).await?; helpers.insert(name.into(),rpc);}
                            let login = helpers[name].request("account/login/start",json!({"type":"chatgptDeviceCode"})).await?;
                            state.accounts.get_mut(name).unwrap().auth_required = true;
                            store.save(&state,now(),"account_login_started",name)?;
                            Ok(json!({"name":name,"login":login}))
                        }
                        "account/enable" | "account/disable" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            state.accounts.get_mut(name).context("unknown account")?.enabled = method == "account/enable";
                            store.save(&state,now(),"account_enabled_changed",name)?; Ok(json!({"saved":true}))
                        }
                        "account/lease" => {
                            let name = params["name"].as_str().context("missing account name")?;
                            ensure!(state.accounts.contains_key(name), "unknown account");
                            ensure!(!pending.contains(name), "account poll in progress; retry shortly");
                            let lease = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW).open(paths.account(name)?.join(".switcher-lease"))?;
                            ensure!(lease.try_lock_exclusive().is_err(), "account lease is not held by launcher");
                            if let Some(helper) = helpers.get(name) { helper.stop_idle().await?; }
                            helpers.remove(name);
                            Ok(json!({"leased":true}))
                        }
                        "account/prepare" | "account/restart-server" => {
                            let name = params["name"].as_str().context("missing account name")?;
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
                let duplicate = result.identity.as_ref().is_some_and(|identity|state.accounts.values().any(|a|a.name != result.name && &a.identity == identity));
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
            Some((at, counts)) = history_rx.recv() => {
                // History is observational; a storage failure must not stop monitoring/accounts.
                if let Err(error) = history.observe(&mut store,&state.accounts,at,config.stale_seconds,counts) {
                    eprintln!("usage history: {error:#}");
                }
            }
            _ = interval.tick() => {
                for a in state.accounts.values() {
                    if !a.enabled || pending.contains(&a.name) { continue; }
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
        .context("supervisor is not running; run `codex-switcher daemon start`")?;
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
    let log_path = paths.root.join("daemon.log");
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
