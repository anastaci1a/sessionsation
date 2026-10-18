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

#[derive(Subcommand)]
pub enum Command {
    Capabilities,
    Launch {
        #[command(subcommand)]
        command: crate::launch::Command,
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
        #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u64).range(1..=3600))]
        interval: u64,
    },
    /// Bounded read-only native readiness; never sends input or starts a process.
    Observe {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
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
            a["selectable"] =
                json!(!removed && a["enabled"] == true && a["auth_required"] == false);
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
                    ensure!(codex_account(&id)?.len() <= 12, "ACCOUNT_NAME_TOO_LONG");
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
    json!({"api_version":VERSION,"providers":[
        {"id":"codex","implemented":true,"enrollment":true,"usage":true,
         "new":true,"resume":true,"account_switch":true,"restart":true,
         "submit":true,"queue":true,"delivery":"non_steering_native_queue",
         "account_removal":true,"account_limits":true,"new_account_name_max":12,"purge":false,"force_interrupt":false,"steer":false,"cross_provider_conversion":false},
        {"id":"claude","implemented":false,"reason":"SETUP_REQUIRED",
         "enrollment":false,"usage":false,"new":false,"resume":false,
         "account_switch":false,"restart":false,"submit":false,"queue":false,
         "force_interrupt":false,"steer":false,"cross_provider_conversion":false}
    ],"launch":{"version":1,"validate":true,"start":true,"status":true},"hosts":["tmux","herdr"],"identity":"stable_backend_session_uuid",
    "mutations_require":"expected_generation","watch":"complete_ndjson_snapshots",
    "session_observation":{"command":"observe","native_timeout_seconds":5,"valid_for_seconds":5,"passive":true},
    "receipt_observation":{"poll_min_seconds":5,"public_correlation":true},
    "detach_stops_agent":false})
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
    let items = external::discover(paths).await?;
    catalog::reconcile(paths, &items)?;
    Ok(items)
}

pub async fn state(paths: &Paths) -> Result<Value> {
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
    let sessions=catalog::list(paths)?.into_iter().map(|s| {
        let source=items.iter().find(|i|i.thread_id==s.conversation_id && i.account==s.account);
        let known=!matches!(s.account.as_str(),"external"|"unknown");
        let mut value=public_session(s);
        let running=source.is_some_and(|i|!i.inactive);
        let saved=source.is_some_and(|i|i.inactive && i.block_reason.is_none() && i.move_id.is_none());
        value["availability"]=json!(if running {"running"}else if saved {"saved"}else{"unavailable"});
        let unambiguous=items.iter().filter(|i|!i.inactive && i.thread_id==value["conversation_id"]).count()<=1;
        value["capabilities"]=json!({"rename":true,"resume":known && saved,"switch_account":known && running && unambiguous,"restart":known && running && unambiguous,"stop":known && running && unambiguous});
        value
    }).collect::<Vec<_>>();
    Ok(json!({"observed_at":daemon::now(),"account_source":source,
        "accounts":accounts,"sessions":sessions,
        "capabilities":capabilities()}))
}

async fn selected(
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
        .find(|i| {
            i.thread_id == session.conversation_id
                && i.account == session.account
                && session.attachment.as_ref().is_none_or(|a| {
                    a.target == i.pane && a.pid == i.pid && a.process_start == i.start_time
                })
        })
        .context("SESSION_UNAVAILABLE")?;
    Ok((session, i))
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
        Command::Capabilities => capabilities(),
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
        Command::State { watch, interval } => loop {
            print(state(paths).await?)?;
            if !watch {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        },
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
        Command::Operations => json!(
            external::jobs(paths)?
                .into_iter()
                .filter(|j| j.non_steering)
                .map(|j| operation(paths, j))
                .collect::<Result<Vec<_>>>()?
        ),
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
        .context("SESSION_NOT_FOUND")?
        .id;
    Ok(
        json!({"id":j.id,"source_session_id":id,"kind":j.kind,"phase":j.phase,"detail":j.detail,
        "confirmation_seq":j.confirmation_seq,"source_account_id":format!("codex:{}",j.source.account),
        "target_account_id":format!("codex:{}",j.target),"provider_conversation_id":j.source.thread_id,
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
