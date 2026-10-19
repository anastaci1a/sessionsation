//! Public machine interface. The inherited CLI and native conversation IDs are
//! implementation details; consumers use stable catalog IDs and generations.
use crate::{catalog, daemon, external, paths::Paths, store};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    time::Duration,
};

pub const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum ArchiveFilter {
    #[default]
    Active,
    Archived,
    All,
}

pub fn version() -> Result<Value> {
    use sha2::{Digest, Sha256};
    let executable = std::env::current_exe()?;
    Ok(
        json!({"package":"sessionsation","package_version":env!("CARGO_PKG_VERSION"),"api_version":VERSION,"contract_revision":2,"executable":{"path":executable,"sha256":format!("{:x}",Sha256::digest(std::fs::read(&executable)?))}}),
    )
}

#[derive(Subcommand)]
pub enum Command {
    Capabilities,
    /// Discover the exact executable and contract without contacting a provider.
    Version,
    Archives,
    Archive {
        #[command(flatten)]
        target: external::archiving::Target,
    },
    RestoreSession {
        #[command(flatten)]
        target: external::archiving::Target,
    },
    DeleteSession {
        #[command(flatten)]
        target: external::archiving::Target,
    },
    Launch {
        #[command(subcommand)]
        command: crate::launch::Command,
    },
    ResumeLaunch {
        #[command(subcommand)]
        command: crate::resume::Command,
    },
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Return an opaque argv plan for a terminal client to exec.
    Launcher {
        #[command(subcommand)]
        command: Launcher,
    },
    State {
        #[arg(long)]
        watch: bool,
        #[arg(long, value_enum, default_value_t=ArchiveFilter::Active)]
        archive_filter: ArchiveFilter,
        #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u64).range(1..=3600))]
        interval: u64,
    },
    /// Resolve public session details against its current catalog generation.
    Session {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    /// Bounded read-only native readiness; never sends input or starts a process.
    Observe {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    /// Bounded batch readiness from stdin; optional cached-only reads never contact a provider.
    Observations,
    Rename {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        name: String,
    },
    Switch {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        accept_risks: bool,
    },
    Restart {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        accept_risks: bool,
    },
    Stop {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    Resume {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        herdr_session: String,
    },
    /// Read existing Spaces from this session's own HERDR host.
    Spaces {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    /// Move only the selected terminal into a new tab in an existing Space.
    MoveSpace {
        #[command(flatten)]
        target: external::spaces::Target,
    },
    Operations,
    Respond {
        operation: String,
        #[arg(long)]
        sequence: u64,
        #[arg(value_parser=["yes","no"])]
        answer: String,
    },
    CancelOperation {
        operation: String,
    },
    Recover {
        operation: String,
    },
    /// Read a versioned Submit/Queue request from stdin; never steers a turn.
    Send,
    Receipt {
        message: String,
    },
    Cancel {
        message: String,
    },
}

#[derive(Subcommand)]
pub enum Launcher {
    New {
        #[arg(long)]
        account: String,
    },
    Resume {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    Shell,
}

pub async fn resume_current(paths: &Paths, id: &str, generation: &str) -> Result<()> {
    let (s, source) = selected(paths, id, generation).await?;
    ensure!(
        source.inactive && s.attachment.is_none(),
        "SESSION_ALREADY_RUNNING"
    );
    ensure!(source.block_reason.is_none(), "SESSION_STATE_UNVERIFIED");
    ensure!(
        !matches!(s.account.as_str(), "external" | "unknown"),
        "ACCOUNT_UNAVAILABLE"
    );
    external::run_account_thread_guarded(
        paths,
        &s.account,
        Some(&s.conversation_id),
        &[],
        Some((id, generation)),
    )
    .await
}

async fn launcher(paths: &Paths, command: Launcher) -> Result<Value> {
    let mut argv = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "--data-dir".into(),
        paths.root.to_string_lossy().into_owned(),
    ];
    let mut cwd = None;
    match command {
        Launcher::New { account } => {
            let name = codex_account(&account)?;
            crate::accounts::ensure_available(paths, name)?;
            let status = daemon::call(paths, "status", json!({})).await?;
            ensure!(
                status["accounts"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["name"] == name
                        && a["enabled"] == true
                        && a["auth_required"] == false)),
                "ACCOUNT_UNAVAILABLE"
            );
            argv.extend(["run".into(), "--account".into(), name.into()]);
        }
        Launcher::Resume {
            session,
            expected_generation,
        } => {
            let (_, source) = selected(paths, &session, &expected_generation).await?;
            ensure!(source.inactive, "SESSION_ALREADY_RUNNING");
            cwd = Some(source.cwd);
            argv.extend([
                "resume".into(),
                session,
                "--expected-generation".into(),
                expected_generation,
            ]);
        }
        Launcher::Shell => argv.push("shell".into()),
    }
    Ok(json!({"action":"exec","argv":argv,"cwd":cwd,"credentials_in_client":false}))
}

#[derive(Subcommand)]
pub enum AccountCommand {
    List,
    Add {
        id: String,
    },
    Login {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Remove {
        id: String,
        #[arg(long, requires = "revision")]
        confirm: Option<String>,
        #[arg(long, requires = "confirm")]
        revision: Option<String>,
    },
    Delete {
        id: String,
        #[arg(long, requires = "request_id")]
        confirm: Option<String>,
        #[arg(long, requires = "confirm")]
        request_id: Option<String>,
    },
    Deletions {
        #[arg(long)]
        request_id: Option<String>,
    },
    Restore {
        id: String,
    },
    Usage {
        id: String,
        #[arg(long)]
        start: i64,
        #[arg(long)]
        end: i64,
        #[arg(long, default_value_t = 1800)]
        step: i64,
    },
    /// Read limits, or explicitly save both used-percentage limits via the owner.
    Limits {
        id: String,
        #[arg(long, requires = "weekly_used")]
        short_used: Option<f64>,
        #[arg(long, requires = "short_used")]
        weekly_used: Option<f64>,
    },
}
fn codex_account(id: &str) -> Result<&str> {
    let name = id
        .strip_prefix("codex:")
        .context("UNSUPPORTED_PROVIDER_OPERATION")?;
    crate::paths::validate_name(name)?;
    Ok(name)
}

pub fn project_accounts(paths: &Paths, accounts: Vec<Value>) -> Result<Vec<Value>> {
    accounts
        .into_iter()
        .map(|mut a| {
            let name = a["name"].as_str().context("INVALID_SUPERVISOR_RESPONSE")?;
            let removed = crate::accounts::removed(paths, name)?;
            a["id"] = json!(format!("codex:{name}"));
            a["provider"] = json!("codex");
            a["membership"] = json!(if removed { "removed" } else { "enrolled" });
            if let Some(id) = a["deletion_id"].as_str() {
                a["deletion"] = json!(crate::account_deletion::status(paths, id)?);
            }
            a["selectable"] = json!(
                !removed
                    && a["deletion_id"].is_null()
                    && a["enabled"] == true
                    && a["auth_required"] == false
            );
            a["prefill"] = account_prefill(
                &a,
                daemon::now(),
                crate::store::load_config(paths)?.stale_seconds,
            );
            Ok(a)
        })
        .collect()
}

async fn account(paths: &Paths, command: AccountCommand) -> Result<Value> {
    match command {
        AccountCommand::Delete {
            id,
            confirm,
            request_id,
        } => {
            let name = codex_account(&id)?;
            if let (Some(confirm), Some(request)) = (confirm, request_id) {
                crate::account_deletion::start(paths, name, &request, &confirm).await
            } else {
                crate::account_deletion::preview(paths, name).await
            }
        }
        AccountCommand::Deletions { request_id } => {
            if let Some(id) = request_id {
                Ok(json!(crate::account_deletion::status(paths, &id)?))
            } else {
                Ok(json!(crate::account_deletion::list(paths)?))
            }
        }
        AccountCommand::Limits {
            id,
            short_used,
            weekly_used,
        } => {
            let name = codex_account(&id)?;
            crate::accounts::ensure_available(paths, name)?;
            if let (Some(short), Some(weekly)) = (short_used, weekly_used) {
                ensure!(
                    [short, weekly]
                        .iter()
                        .all(|n| n.is_finite() && (0. ..=100.).contains(n)),
                    "INVALID_ACCOUNT_LIMIT"
                );
                // Delegate to the existing single writer; never edit account files
                // or start/replace a supervisor just to save preferences.
                let value = daemon::call(
                    paths,
                    "account/thresholds",
                    json!({"name":name,"short":short,"weekly":weekly}),
                )
                .await?;
                Ok(json!({"account_id":id,"limits":value["thresholds"],"saved":value["saved"]}))
            } else {
                let status = daemon::call(paths, "status", json!({})).await?;
                let a = status["accounts"]
                    .as_array()
                    .and_then(|items| items.iter().find(|a| a["name"] == name))
                    .context("ACCOUNT_NOT_FOUND")?;
                Ok(json!({"account_id":id,"limits":a["switch_thresholds"],"saved":false}))
            }
        }
        AccountCommand::List => {
            let v = daemon::call(paths, "status", json!({})).await?;
            Ok(json!(project_accounts(
                paths,
                v["accounts"]
                    .as_array()
                    .context("INVALID_SUPERVISOR_RESPONSE")?
                    .clone()
            )?))
        }
        AccountCommand::Remove {
            id,
            confirm,
            revision,
        } => {
            let name = codex_account(&id)?;
            if let (Some(confirm), Some(revision)) = (confirm, revision) {
                crate::accounts::remove(paths, name, &revision, &confirm).await
            } else {
                crate::accounts::preview(paths, name).await
            }
        }
        AccountCommand::Restore { id } => crate::accounts::restore(paths, codex_account(&id)?),
        AccountCommand::Usage {
            id,
            start,
            end,
            step,
        } => {
            ensure!(
                start < end && (60..=86400).contains(&step),
                "INVALID_HISTORY_RANGE"
            );
            daemon::call(
                paths,
                "account/history",
                json!({"name":codex_account(&id)?,"start":start,"end":end,"step":step}),
            )
            .await
        }
        command => {
            let (method, id) = match command {
                AccountCommand::Add { id } => {
                    ensure!(codex_account(&id)?.len() <= 24, "ACCOUNT_NAME_TOO_LONG");
                    ("account/add", id)
                }
                AccountCommand::Login { id } => ("account/login", id),
                AccountCommand::Enable { id } => ("account/enable", id),
                AccountCommand::Disable { id } => ("account/disable", id),
                _ => unreachable!(),
            };
            let name = codex_account(&id)?;
            crate::accounts::ensure_available(paths, name)?;
            daemon::ensure_started(paths).await?;
            daemon::call(paths, method, json!({"name":name})).await
        }
    }
}

pub fn capabilities() -> Value {
    json!({"api_version":VERSION,"package_version":env!("CARGO_PKG_VERSION"),"contract_revision":2,"version_discovery":true,"archive_filter":{"values":["active","archived","all"],"default":"active","unknown_retained":true},"providers":[
        {"id":"codex","implemented":true,"enrollment":true,"usage":true,
         "new":true,"resume":true,"account_switch":true,"restart":true,
         "submit":true,"queue":true,"delivery":"non_steering_native_queue",
         "account_deletion":true,"account_removal":true,"account_limits":true,"new_account_name_max":24,"purge":false,"force_interrupt":false,"steer":false,"cross_provider_conversion":false},
        {"id":"claude","implemented":false,"reason":"SETUP_REQUIRED",
         "enrollment":false,"usage":false,"new":false,"resume":false,
         "account_switch":false,"restart":false,"submit":false,"queue":false,
         "force_interrupt":false,"steer":false,"cross_provider_conversion":false}
    ],"launch":{"version":1,"validate":true,"start":true,"status":true,"list":true},"resume":{"version":1,"unfinished_confirmation":true,"directory_selection":true,"validate":true,"start":true,"status":true,"list":true},"hosts":["tmux","herdr"],"identity":"stable_backend_session_uuid",
    "mutations_require":"expected_generation","watch":"complete_ndjson_snapshots",
    "batch_observation":{"command":"observations","max_targets":64,"concurrency":8,"native_budget_seconds":5,"policies":["if_stale","refresh","cached_only"],"cache_hits_renew_evidence":false},
    "session_observation":{"command":"observe","native_timeout_seconds":5,"valid_for_seconds":5,"passive":true},
    "receipt_observation":{"poll_min_seconds":5,"public_correlation":true},
    "conversation_archive":{"version":1,"list":true,"archive":true,"restore":true,"delete":true,"compression":"gzip","storage":"sessionsation_data_root"},
    "space_move":{"version":1,"host":"herdr","existing_spaces":true,"preserves_process":true,"recovery":"read_only_reconciliation"},"session_details":true,"session_timing":true,"detach_stops_agent":false})
}

fn public_session(s: catalog::Session) -> Value {
    let generation = s.generation();
    let account_id = format!("{}:{}", s.provider, s.account);
    let mut value = json!(s);
    value["account_id"] = json!(account_id);
    value["generation"] = json!(generation);
    value
}

async fn inventory(paths: &Paths) -> Result<Vec<external::Instance>> {
    let items = external::discover_catalog(paths).await?;
    catalog::reconcile(paths, &items)?;
    Ok(items)
}
fn source_matches(s: &catalog::Session, i: &external::Instance) -> bool {
    i.thread_id == s.conversation_id
        && i.account == s.account
        && s.attachment.as_ref().is_none_or(|a| {
            !i.inactive && a.target == i.pane && a.pid == i.pid && a.process_start == i.start_time
        })
}

fn attachment_started_at(now: i64, uptime: f64, ticks: u64, hz: i64) -> Option<i64> {
    if hz <= 0 || ticks == 0 || !uptime.is_finite() {
        return None;
    }
    let age = uptime - ticks as f64 / hz as f64;
    (age >= 0. && age <= now as f64).then(|| now - age.floor() as i64)
}
fn boot_clock() -> Option<(f64, i64)> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Linux's process-start ticks and CLOCK_BOOTTIME share the boot origin.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) };
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (result == 0 && hz > 0).then_some((time.tv_sec as f64 + time.tv_nsec as f64 / 1e9, hz))
}

fn archive_info(
    s: &catalog::Session,
    source: Option<&external::Instance>,
    archives: &std::collections::BTreeMap<String, Value>,
) -> Value {
    archives.get(&s.id).cloned().unwrap_or_else(|| match source {
        Some(i) => { let archived=i.archived; json!({"kind":if archived {"provider"} else {"none"},"archived":archived,"state":if archived {"archived"} else {"active"},"archived_at":null,"source":"verified_history_location","provider_storage_archived":i.rollout.starts_with(i.home.join("archived_sessions"))}) },
        None => json!({"kind":"unknown","archived":null,"state":"unobserved","archived_at":null,"source":"catalog_only"}),
    })
}
pub async fn state(paths: &Paths) -> Result<Value> {
    state_filtered(paths, ArchiveFilter::Active).await
}
pub async fn state_filtered(paths: &Paths, filter: ArchiveFilter) -> Result<Value> {
    let items = inventory(paths).await?;
    let status = daemon::call(paths, "status", json!({})).await;
    let (accounts, source) = match status {
        Ok(v) => (
            v["accounts"]
                .as_array()
                .context("INVALID_SUPERVISOR_RESPONSE")?
                .clone(),
            "supervisor",
        ),
        Err(_) if paths.root.join("registry.db").exists() => {
            let state = store::load_state_snapshot(paths)?;
            let config = store::load_config(paths)?;
            let v = daemon::status(paths, &state, &config, "unavailable");
            (
                v["accounts"]
                    .as_array()
                    .context("INVALID_SUPERVISOR_RESPONSE")?
                    .clone(),
                "offline_snapshot",
            )
        }
        Err(_) => (vec![], "uninitialized"),
    };
    let accounts = project_accounts(paths, accounts)?;
    let clock = boot_clock();
    let now = daemon::now();
    let archives = external::archiving::metadata(paths)?;
    let mut sessions=catalog::list(paths)?.into_iter().filter(|s|archives.get(&s.id).is_none_or(|a|a["state"]!="deleted")).map(|s| {
        let source=items.iter().find(|i|source_matches(&s,i));
        let known=!matches!(s.account.as_str(),"external"|"unknown");
        let attached=s.attachment.is_some();
        let archive = archive_info(&s, source, &archives);
        let archived=archive["archived"]==true;
        let mut value=public_session(s);
        value["archive"]=archive;
        value["archived"]=value["archive"]["archived"].clone();
        value["resume_block_reason"]=json!(if attached && source.is_none() {Some("host_unavailable")}else{resume_block_reason(known,source)});
        let running=source.is_some_and(|i|!i.inactive);
        let saved=source.is_some_and(|i|i.inactive && matches!(i.block_reason.as_deref(), None | Some("unfinished" | "unknown")) && i.move_id.is_none());
        value["availability"]=json!(if archived {"archived"}else if running {"running"}else if saved {"saved"}else{"unavailable"});
        let facts=source.map(|s|external::history_file_details(&s.rollout)).unwrap_or(Value::Null);
        value["directory"]=source.map(|s|json!(s.cwd)).unwrap_or(Value::Null);
        value["conversation_created_at"]=facts["conversation_created_at"].clone();
        value["history_file_bytes"]=facts["history_file_bytes"].clone();
        value["activity"]=source.map(|s|json!(s.activity)).unwrap_or(Value::Null);
        value["last_accessed"]=source.and_then(|s|s.last_accessed).map_or(Value::Null,|n|json!(n));
        value["active_since"]=json!(source.filter(|s|!s.inactive).and_then(|s|clock.and_then(|(uptime,hz)|attachment_started_at(now,uptime,s.start_time,hz))));
        let unambiguous=items.iter().filter(|i|!i.inactive && i.thread_id==value["conversation_id"]).count()<=1;
        value["capabilities"]=json!({"rename":true,"move_space":running && unambiguous && source.is_some_and(|s|s.pane.starts_with("herdr/")),"resume":known && saved,"switch_account":known && running && unambiguous,"restart":known && running && unambiguous,"stop":known && running && unambiguous,"archive":source.is_some_and(|i|i.inactive),"delete":source.is_some_and(|i|i.inactive)});
        if archived { value["capabilities"]=json!({"rename":false,"resume":false,"switch_account":false,"move_space":false,"restart":false,"stop":false,"archive":false,"delete":value["archive"]["kind"]=="managed","restore":value["archive"]["kind"]=="managed"}); }
        value
    }).filter(|s|match filter { ArchiveFilter::All=>true, ArchiveFilter::Archived=>s["archived"]==true, ArchiveFilter::Active=>s["archived"]!=true }).collect::<Vec<_>>();
    for session in &mut sessions {
        let thread = session["conversation_id"]
            .as_str()
            .context("INVALID_CONVERSATION_ID")?;
        let pending = crate::lifecycle::pending(paths, thread, None)?.or_else(|| {
            items
                .iter()
                .find(|i| i.thread_id == thread)
                .and_then(|i| i.move_id.clone())
        });
        if let Some(operation) = pending {
            session["pending_operation_id"] = json!(operation);
            for capability in [
                "resume",
                "switch_account",
                "move_space",
                "restart",
                "stop",
                "archive",
                "delete",
            ] {
                session["capabilities"][capability] = json!(false);
            }
            session["resume_block_reason"] = json!("moving");
        }
    }
    Ok(json!({"observed_at":daemon::now(),"account_source":source,
        "accounts":accounts,"sessions":sessions,
        "capabilities":capabilities()}))
}

pub(crate) async fn selected(
    paths: &Paths,
    id: &str,
    generation: &str,
) -> Result<(catalog::Session, external::Instance)> {
    let items = inventory(paths).await?;
    let session = catalog::resolve(paths, id, generation)?;
    ensure!(
        session.provider == "codex",
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    ensure!(
        items
            .iter()
            .filter(|i| !i.inactive && i.thread_id == session.conversation_id)
            .count()
            <= 1,
        "AMBIGUOUS_SESSION"
    );
    let i = items
        .into_iter()
        .find(|i| source_matches(&session, i))
        .context("SESSION_UNAVAILABLE")?;
    ensure!(
        !i.archived && !external::archiving::hidden(paths)?.contains(id),
        "SESSION_ARCHIVED"
    );
    Ok((session, i))
}

/// Public reason for an unavailable resume; observations are rechecked at launch.
fn resume_block_reason(known: bool, source: Option<&external::Instance>) -> Option<&'static str> {
    if !known {
        return Some("unknown_account");
    }
    let Some(source) = source else {
        return Some("history_unavailable");
    };
    if source.archived {
        return Some("archived");
    }
    if !source.inactive {
        return Some("already_running");
    }
    if source.move_id.is_some() {
        return Some("moving");
    }
    match source.block_reason.as_deref() {
        None => None,
        Some("unknown") => Some("missing_directory"),
        Some("busy") => Some("history_busy"),
        Some("unfinished") => Some("unfinished_history"),
        Some("diverged") => Some("conflicting_history"),
        Some("moving") => Some("moving"),
        Some(_) => Some("unverified_history"),
    }
}

pub async fn session_details(paths: &Paths, id: &str, generation: &str) -> Result<Value> {
    let items = inventory(paths).await?;
    let session = catalog::resolve(paths, id, generation)?;
    let source = items.iter().find(|i| source_matches(&session, i));
    let directory = source.map(|s| s.cwd.clone());
    let reason = if session.attachment.is_some() && source.is_none() {
        Some("host_unavailable")
    } else {
        resume_block_reason(
            !matches!(session.account.as_str(), "external" | "unknown"),
            source,
        )
    };
    let archive = archive_info(&session, source, &external::archiving::metadata(paths)?);
    let archived = archive["archived"] == true;
    let mut value = public_session(session);
    value["archived"] = archive["archived"].clone();
    value["archive"] = archive;
    value["directory"] = json!(directory);
    value["resume_block_reason"] = json!(if archived { Some("archived") } else { reason });
    let facts = source
        .map(|s| external::history_file_details(&s.rollout))
        .unwrap_or(Value::Null);
    value["conversation_created_at"] = facts["conversation_created_at"].clone();
    value["history_file_bytes"] = facts["history_file_bytes"].clone();
    value["availability"] = if archived {
        json!("archived")
    } else {
        json!(
            source
                .map(|s| if s.inactive { "saved" } else { "running" })
                .unwrap_or("unavailable")
        )
    };
    // Persisted launch receipts supply positive location provenance. Never guess
    // the original Space from a display name, current focus or a missing pane.
    let latest = external::spaces::receipt_location(paths, id)?;
    value["original_host"] = if let Some((epoch, location)) = latest {
        external::spaces::follow_location(paths, id, location, Some(&epoch))?
    } else {
        external::spaces::last_location(paths, id)?
    };
    Ok(value)
}

pub fn print(result: Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(
        &mut out,
        &json!({"api_version":VERSION,"ok":true,"result":result}),
    )?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

pub async fn run(paths: &Paths, command: Command) -> Result<()> {
    let result = match command {
        Command::ResumeLaunch { command } => crate::resume::api(paths, command).await?,
        Command::Session {
            session,
            expected_generation,
        } => session_details(paths, &session, &expected_generation).await?,
        Command::Capabilities => capabilities(),
        Command::Version => version()?,
        Command::Archives => external::archiving::list(paths)?,
        Command::Archive { target } => {
            external::archiving::start(paths, external::archiving::Kind::Archive, target).await?
        }
        Command::RestoreSession { target } => {
            external::archiving::start(paths, external::archiving::Kind::Restore, target).await?
        }
        Command::DeleteSession { target } => {
            external::archiving::start(paths, external::archiving::Kind::Delete, target).await?
        }
        Command::Launch { command } => crate::launch::api(paths, command).await?,
        Command::Account { command } => account(paths, command).await?,
        Command::Launcher { command } => launcher(paths, command).await?,
        Command::Observe {
            session,
            expected_generation,
        } => {
            selected(paths, &session, &expected_generation).await?;
            crate::messaging::observe_public(paths, &session, &expected_generation).await?
        }
        Command::State {
            watch,
            interval,
            archive_filter,
        } => loop {
            print(state_filtered(paths, archive_filter).await?)?;
            if !watch {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        },
        Command::Observations => {
            let mut bytes = Vec::new();
            std::io::stdin().take(32769).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 32768, "INPUT_TOO_LARGE");
            crate::messaging::observations(
                paths,
                serde_json::from_slice(&bytes).context("INVALID_REQUEST")?,
            )
            .await?
        }
        Command::Rename {
            session,
            expected_generation,
            name,
        } => {
            inventory(paths).await?;
            public_session(catalog::rename(
                paths,
                &session,
                &expected_generation,
                &name,
            )?)
        }
        Command::Switch {
            session,
            expected_generation,
            to,
            accept_risks,
        } => {
            let (_, source) = selected(paths, &session, &expected_generation).await?;
            let target = to
                .strip_prefix("codex:")
                .context("ACCOUNT_PROVIDER_REQUIRED")?;
            ensure!(!source.inactive, "SESSION_NOT_RUNNING");
            operation(
                paths,
                external::queue_snapshot(
                    paths,
                    source,
                    target,
                    external::MoveOptions {
                        pause: false,
                        accept_risks,
                    },
                    "account",
                )
                .await?,
            )?
        }
        Command::Restart {
            session,
            expected_generation,
            accept_risks,
        } => {
            let (_, source) = selected(paths, &session, &expected_generation).await?;
            ensure!(!source.inactive, "SESSION_NOT_RUNNING");
            operation(
                paths,
                external::queue_snapshot(
                    paths,
                    source,
                    "",
                    external::MoveOptions {
                        pause: false,
                        accept_risks,
                    },
                    "restart",
                )
                .await?,
            )?
        }
        Command::Stop {
            session,
            expected_generation,
        } => {
            let (_, source) = selected(paths, &session, &expected_generation).await?;
            ensure!(!source.inactive, "SESSION_NOT_RUNNING");
            operation(
                paths,
                external::queue_snapshot(
                    paths,
                    source,
                    "",
                    external::MoveOptions::default(),
                    "terminate",
                )
                .await?,
            )?
        }
        Command::Resume {
            session,
            expected_generation,
            herdr_session,
        } => {
            let (s, source) = selected(paths, &session, &expected_generation).await?;
            ensure!(
                source.inactive && s.attachment.is_none(),
                "SESSION_ALREADY_RUNNING"
            );
            ensure!(source.block_reason.is_none(), "SESSION_STATE_UNVERIFIED");
            ensure!(
                !matches!(s.account.as_str(), "external" | "unknown"),
                "ACCOUNT_UNAVAILABLE"
            );
            external::hosting::launch_checked(
                paths,
                &herdr_session,
                &s.account,
                Some(&s.conversation_id),
                &[],
                (&session, &expected_generation),
            )
            .await?
        }
        Command::Spaces {
            session,
            expected_generation,
        } => external::spaces::list(paths, &session, &expected_generation).await?,
        Command::MoveSpace { target } => external::spaces::start(paths, target).await?,
        Command::Operations => {
            let mut ops = external::jobs(paths)?
                .into_iter()
                .filter(|j| j.non_steering)
                .map(|j| operation(paths, j))
                .collect::<Result<Vec<_>>>()?;
            ops.extend(
                crate::account_deletion::list(paths)?
                    .into_iter()
                    .map(|r| json!(r)),
            );
            ops.extend(external::archiving::operations(paths)?);
            ops.extend(external::spaces::operations(paths)?);
            json!(ops)
        }
        Command::Respond {
            operation: id,
            sequence,
            answer,
        } => {
            managed_operation(paths, &id)?;
            external::respond_move_sequence(paths, &id, answer == "yes", Some(sequence))?;
            json!({"id":id,"response_recorded":true})
        }
        Command::CancelOperation { operation: id } => {
            managed_operation(paths, &id)?;
            external::cancel_move(paths, &id)?;
            json!({"id":id,"cancellation_requested":true})
        }
        Command::Recover { operation: id } if external::spaces::exists(paths, &id) => {
            external::spaces::recover(paths, &id)?
        }
        Command::Recover { operation: id }
            if crate::account_deletion::status(paths, &id).is_ok() =>
        {
            crate::account_deletion::recover(paths, &id)?
        }
        Command::Recover { operation: id } => {
            managed_operation(paths, &id)?;
            operation(paths, external::recover(paths, &id).await?)?
        }
        Command::Send => {
            let mut bytes = Vec::new();
            std::io::stdin().take(32769).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 32768, "INPUT_TOO_LARGE");
            let request: crate::messaging::PublicRequest =
                serde_json::from_slice(&bytes).context("INVALID_REQUEST")?;
            inventory(paths).await?;
            crate::messaging::send_public(paths, request).await?
        }
        Command::Receipt { message } => {
            crate::messaging::receipt_public(paths, &message, false).await?
        }
        Command::Cancel { message } => {
            crate::messaging::receipt_public(paths, &message, true).await?
        }
    };
    print(result)
}

fn operation(paths: &Paths, j: external::Job) -> Result<Value> {
    let id = catalog::list(paths)?
        .into_iter()
        .find(|s| s.provider == "codex" && s.conversation_id == j.source.thread_id)
        .map(|s| s.id);
    Ok(
        json!({"id":j.id,"source_session_id":id,"kind":j.kind,"phase":j.phase,"detail":j.detail,
        "confirmation_seq":j.confirmation_seq,"source_account_id":format!("codex:{}",j.source.account),
        "target_account_id":format!("codex:{}",j.target),"provider_conversation_id":j.source.thread_id,
        "created_at":j.created_at,
        "can_cancel":external::can_cancel_move(&j),
        "can_recover": crate::background::worker_stopped(&paths.root.join("handoff-jobs").join(format!("{}.worker-lock", j.id)))? && (j.phase == "failed" || (!matches!(j.phase.as_str(), "complete" | "cancelled" | "recovered") && daemon::now() - j.created_at > 10)),
        "can_respond":j.phase.starts_with("awaiting_"),
        "non_steering":j.non_steering}),
    )
}
fn managed_operation(paths: &Paths, id: &str) -> Result<external::Job> {
    let job = external::jobs(paths)?
        .into_iter()
        .find(|j| j.id == id)
        .context("OPERATION_NOT_FOUND")?;
    ensure!(job.non_steering, "OPERATION_NOT_MANAGED");
    Ok(job)
}

/// Conservative preference policy, separate from permission to explicitly launch.
pub fn account_prefill(a: &Value, now: i64, stale_seconds: u64) -> Value {
    let usage = &a["usage"];
    let mut until = usage["observed_at"]
        .as_i64()
        .unwrap_or(0)
        .saturating_add(stale_seconds as i64)
        .min(now + 5);
    let mut reason = if a["selectable"] != true {
        Some("ACCOUNT_UNAVAILABLE")
    } else if a["usage_fresh"] != true {
        Some("USAGE_STALE")
    } else {
        None
    };
    if a["switch_thresholds"]["needs_plan_review"] != false {
        reason = Some("LIMITS_UNVERIFIED");
    }
    for (window, limit) in [("short", "short_percent"), ("weekly", "weekly_percent")] {
        if !usage[window].is_null() {
            until = until.min(usage[window]["resets_at"].as_i64().unwrap_or(0));
            if !usage[window]["used_percent"]
                .as_f64()
                .zip(a["switch_thresholds"][limit].as_f64())
                .is_some_and(|(used, ceiling)| used.is_finite() && used < ceiling)
            {
                reason = Some("LIMIT_REACHED");
            }
        }
    }
    if usage["exhausted"] == true
        || usage["ordinary_usage_allowed"] == false
        || usage["spend_control_reached"] == true
    {
        reason = Some("ALLOWANCE_UNAVAILABLE");
    }
    if until <= now {
        reason = Some("USAGE_STALE");
    }
    json!({"eligible":reason.is_none(),"reason":reason,"observed_at":now,"valid_until":until})
}
#[cfg(test)]
mod prefill_tests {
    use super::*;
    #[test]
    fn an_unobserved_attachment_is_not_an_inactive_history_source() {
        let s = catalog::Session {
            id: "stable".into(),
            name: "Name".into(),
            provider: "codex".into(),
            account: "selected".into(),
            conversation_id: "thread".into(),
            revision: 1,
            attachment: Some(catalog::Attachment {
                host: "herdr".into(),
                target: "herdr/named/w1:p1".into(),
                pid: 10,
                process_start: 20,
            }),
        };
        let mut source: external::Instance = serde_json::from_value(json!({"inactive":true,"account":"selected","pane":"","tmux_session":"","pid":0,"cwd":"/tmp","thread_id":"thread","rollout":"/tmp/fixture","home":"/tmp/home","binary":"codex","resume_options":[]})).unwrap();
        assert!(!source_matches(&s, &source));
        source.inactive = false;
        source.pane = "herdr/named/w1:p1".into();
        source.pid = 10;
        source.start_time = 20;
        assert!(source_matches(&s, &source));
        source.start_time += 1;
        assert!(!source_matches(&s, &source));
    }
    #[test]
    fn resume_reasons_explain_observed_safety_gates() {
        let mut source: external::Instance = serde_json::from_value(json!({
            "inactive":true,"pane":"","tmux_session":"","pid":0,"cwd":"/tmp",
            "thread_id":"fixture","rollout":"/tmp/history","home":"/tmp/home",
            "binary":"codex","resume_options":[]
        }))
        .unwrap();
        assert_eq!(
            resume_block_reason(false, Some(&source)),
            Some("unknown_account")
        );
        assert_eq!(resume_block_reason(true, None), Some("history_unavailable"));
        assert_eq!(resume_block_reason(true, Some(&source)), None);
        for (block, expected) in [
            ("unknown", "missing_directory"),
            ("busy", "history_busy"),
            ("unfinished", "unfinished_history"),
            ("diverged", "conflicting_history"),
            ("moving", "moving"),
            ("future", "unverified_history"),
        ] {
            source.block_reason = Some(block.into());
            assert_eq!(resume_block_reason(true, Some(&source)), Some(expected));
        }
        source.block_reason = None;
        source.move_id = Some("operation".into());
        assert_eq!(resume_block_reason(true, Some(&source)), Some("moving"));
        source.inactive = false;
        assert_eq!(
            resume_block_reason(true, Some(&source)),
            Some("already_running")
        );
    }
    #[test]
    fn session_elapsed_uses_current_attachment_clock_and_rejects_unknown_ticks() {
        assert_eq!(attachment_started_at(1000, 120., 6000, 100), Some(940));
        assert_eq!(attachment_started_at(1000, 120., 0, 100), None);
        assert_eq!(attachment_started_at(1000, 120., 13000, 100), None);
        assert_eq!(attachment_started_at(1000, 120., 6000, 0), None);
    }
    #[test]
    fn stale_thresholds_and_credits_do_not_auto_select() {
        let mut a = json!({"selectable":true,"usage_fresh":true,"switch_thresholds":{"needs_plan_review":false,"short_percent":90.,"weekly_percent":90.},"usage":{"observed_at":100,"short":{"used_percent":20.,"resets_at":500},"weekly":{"used_percent":20.,"resets_at":500}}});
        assert_eq!(account_prefill(&a, 101, 210)["eligible"], true);
        a["usage"]["short"]["used_percent"] = json!(90.);
        assert_eq!(account_prefill(&a, 101, 210)["eligible"], false);
        assert_eq!(a["selectable"], true);
        a["usage"]["short"]["used_percent"] = json!(20.);
        a["usage"]["credits"] = json!({"has_credits":true});
        a["usage"]["exhausted"] = json!(true);
        assert_eq!(account_prefill(&a, 101, 210)["eligible"], false);
        a["usage"]["exhausted"] = json!(false);
        assert_eq!(account_prefill(&a, 311, 210)["eligible"], false);
    }
}
