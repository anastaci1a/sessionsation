use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};
use sessionsation::{
    compatibility,
    config::Config,
    daemon, external,
    model::State,
    paths::{Paths, atomic_write, private_dir},
    simulation::{self, Input},
    store,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Local Codex account monitor and handoff development harness",
    long_about = "Local Codex account monitor and durable scheduling/handoff harness. Native automatic switching is gated pending compatibility verification. See doctor --probe and docs/COMPATIBILITY.md."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Private state directory; defaults to SESSIONSATION_HOME or ~/.local/share/sessionsation"
    )]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Transport-neutral, attach-only messaging API (JSON).
    Message {
        #[command(subcommand)]
        command: sessionsation::messaging::Command,
    },
    /// Initialize private state and start the metadata-only supervisor.
    Setup {
        #[arg(long)]
        systemd: bool,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Exit successfully only when managed native launching is available (currently unavailable).
    Ready,
    /// List live tmux instances and completed inactive conversations available to move.
    Instances,
    /// Inspect one exact inventory ID, its account usage and automation settings (JSON).
    Session {
        #[arg(long)]
        session: String,
    },
    /// List archived conversations available for explicit restoration (JSON).
    Archives,
    /// Open a live or saved inventory ID in tmux, without sending a prompt (JSON).
    Open {
        #[arg(long)]
        session: String,
        /// Name for a newly created tmux session; existing live sessions are reused.
        #[arg(long)]
        name: Option<String>,
    },
    /// Rename a live or saved conversation; the name survives moves and reopening.
    Rename {
        #[arg(long)]
        session: String,
        #[arg(long)]
        name: String,
    },
    /// Preview permanent deletion of an inactive conversation and its local history copies.
    DeleteSession {
        #[arg(long)]
        session: String,
        /// Exact conversation UUID from the preview. Omit for a read-only preview.
        #[arg(long, requires = "revision")]
        confirm: Option<String>,
        /// Preview revision; rejects confirmation after history changes.
        #[arg(long, requires = "confirm")]
        revision: Option<String>,
    },
    /// Read the menu-facing session inventory as JSON (live tmux sessions by default).
    Sessions {
        /// Include ordinary and exited tmux panes, without Codex management actions.
        #[arg(long)]
        include_tmux: bool,
        /// Include deduplicated saved conversations.
        #[arg(long)]
        all: bool,
        /// Stream complete snapshots, one JSON object per line, until interrupted.
        #[arg(long)]
        watch: bool,
        /// Poll interval in seconds for --watch.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=3600))]
        interval: u64,
    },
    /// Show queued or completed manual pane moves.
    Moves,
    /// Move a live pane or completed inactive thread to another account.
    Move {
        #[arg(long, help = "Live tmux pane ID from instances")]
        pane: Option<String>,
        #[arg(
            long,
            conflicts_with = "pane",
            help = "Inactive thread ID from instances"
        )]
        thread: Option<String>,
        #[arg(long)]
        to: String,
        /// Approve sending a pause request if the conversation is working.
        #[arg(long)]
        pause: bool,
        /// Acknowledge history/launch/capacity limitations reported by the move.
        #[arg(long)]
        accept_risks: bool,
    },
    /// Answer the current confirmation dialog for a queued move.
    MoveResponse {
        job: String,
        #[arg(value_parser = ["yes", "no"])]
        answer: String,
        /// Reject an answer to a dialog that has changed since it was displayed.
        #[arg(long)]
        sequence: Option<u64>,
    },
    /// Cancel a queued move before the source restart begins.
    CancelMove {
        job: String,
    },
    /// Restart a live Codex terminal using the installed binary and its exact conversation.
    Restart {
        #[arg(long)]
        pane: String,
        #[arg(long)]
        pause: bool,
        #[arg(long)]
        accept_risks: bool,
    },
    /// Pause a live session and clean up approved background work, leaving Codex open.
    PauseSession {
        #[arg(long)]
        pane: String,
        /// Preapprove the pause message; background termination still needs separate consent.
        #[arg(long)]
        pause: bool,
        #[arg(long)]
        accept_risks: bool,
    },
    /// End an active Codex session, retaining its conversation for resume.
    Terminate {
        #[arg(long)]
        pane: String,
        /// Approve sending the pause request; background termination still needs separate consent.
        #[arg(long)]
        pause: bool,
        #[arg(long)]
        accept_risks: bool,
    },
    /// Change a conversation's working directory, optionally moving the project folder.
    Relocate {
        #[arg(long, conflicts_with = "thread")]
        pane: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        to: PathBuf,
        /// Move the folder itself; otherwise --to must already exist.
        #[arg(long)]
        move_files: bool,
        /// Return a plan and affected sessions without changing anything.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        pause: bool,
        #[arg(long)]
        accept_risks: bool,
    },
    /// Respond to a queued move's background-process termination prompt.
    BackgroundResponse {
        job: String,
        #[arg(value_parser = ["yes", "no"])]
        answer: String,
    },
    /// Relaunch a failed move from the original account home.
    Recover {
        job: String,
    },
    /// Retry a stopped job with fresh session discovery and new confirmations.
    Retry {
        job: String,
    },
    #[command(hide = true)]
    HandoffWorker {
        #[arg(long)]
        job: String,
    },
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Configure experimental automatic moves, or disable them immediately.
    AutoSwitch {
        #[arg(long)]
        thread: String,
        /// Comma-separated account order. Pass an empty string to clear.
        #[arg(long)]
        accounts: Option<String>,
        #[arg(long, default_value = "")]
        current_account: String,
        /// Opt in to automatic pause/move/continuation using the manual move worker.
        #[arg(long, requires = "accept_experimental", conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        accept_experimental: bool,
    },
    /// Resume unfinished work after a confirmed usage stop and verified reset.
    AutoStart {
        #[arg(long)]
        thread: String,
        #[arg(long, requires = "accept_experimental", conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        accept_experimental: bool,
    },
    /// Set an account's used-percentage thresholds or confirm its plan ratio.
    Thresholds {
        account: String,
        #[arg(long, requires = "weekly")]
        short: Option<f64>,
        #[arg(long, requires = "short")]
        weekly: Option<f64>,
        #[arg(long)]
        multiplier: Option<f64>,
    },
    /// Report native compatibility; --probe starts only isolated, unauthenticated workers.
    Doctor {
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run a deterministic offline scenario. Never starts Codex or reads credentials.
    Simulate {
        scenario: PathBuf,
    },
    /// Start Codex with a selected enrolled account; managed switching remains gated.
    Run {
        #[arg(long)]
        account: Option<String>,
        /// Resume an exact conversation under the selected account.
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(last = true)]
        codex_options: Vec<String>,
    },
    /// Reserved conversation import; refuses unsafe copying while compatibility is unverified.
    Import {
        #[arg(long)]
        thread: String,
        #[arg(long)]
        from: PathBuf,
    },
    Pause {
        session: String,
    },
    Resume {
        session: String,
    },
}
#[derive(Subcommand)]
enum DaemonCommand {
    Start,
    Serve,
    Stop,
}
#[derive(Subcommand)]
enum AccountCommand {
    Add {
        name: String,
    },
    Login {
        name: String,
    },
    List,
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    /// Read account details, plan, limits and remaining percentages without starting a monitor.
    Show {
        name: String,
    },
    /// Export the same usage/activity bins shown by the account history UI (JSON).
    History {
        name: String,
        #[arg(long, value_parser = ["hourly", "weekly"], conflicts_with_all = ["hours", "start"])]
        range: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=2160), conflicts_with = "start")]
        hours: Option<u32>,
        /// Inclusive Unix-second range start; requires --end.
        #[arg(long, requires = "end")]
        start: Option<i64>,
        /// Exclusive Unix-second range end; defaults to the next half-hour boundary.
        #[arg(long)]
        end: Option<i64>,
        /// Bin width in seconds, a multiple of 1800; at most 168 bins.
        #[arg(long)]
        step: Option<i64>,
    },
}
#[derive(Subcommand)]
enum ConfigCommand {
    Get,
    Set { key: String, value: String },
}

#[tokio::main]
async fn main() {
    unsafe {
        libc::umask(0o077);
    }
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("codex-switcher: {error:#}");
        std::process::exit(1);
    }
}
async fn run(cli: Cli) -> Result<()> {
    // Offline simulation deliberately avoids creating a daemon or accessing user state.
    if let Commands::Simulate { scenario } = &cli.command {
        return simulate(scenario);
    }
    let paths = Paths::new(cli.data_dir)?;
    match cli.command {
        Commands::Session { session } => {
            print_json(&sessionsation::cli_views::session(&paths, &session).await?)?;
        }
        Commands::Account {
            command: AccountCommand::Show { name },
        } => {
            let status = daemon::call(&paths, "status", json!({})).await?;
            print_json(&sessionsation::cli_views::account(&status, &name)?)?;
        }
        Commands::Account {
            command:
                AccountCommand::History {
                    name,
                    range,
                    hours,
                    start,
                    end,
                    step,
                },
        } => {
            let at = daemon::now();
            let hours = hours.unwrap_or(if range.as_deref() == Some("weekly") {
                168
            } else {
                12
            });
            let (start, end, step) =
                sessionsation::cli_views::history_range(at, hours, start, end, step)?;
            let history = daemon::call(
                &paths,
                "account/history",
                json!({"name":name,"start":start,"end":end,"step":step}),
            )
            .await?;
            print_json(
                &json!({"schema_version":1,"observed_at":at,"range":{"start":start,"end":end,"step":step},"history":history}),
            )?;
        }
        Commands::Message { command } => {
            if !sessionsation::messaging::run(&paths, command).await {
                std::process::exit(1);
            }
        }
        Commands::Setup { systemd } => {
            if !paths.root.join("config.json").exists() {
                store::save_config(&paths, &Config::default())?;
            }
            for directory in ["accounts", "sessions"] {
                private_dir(&paths.root.join(directory))?;
            }
            if systemd {
                install_systemd(&paths)?;
                return Ok(());
            }
            daemon::ensure_started(&paths).await?;
            println!(
                "Initialized {}\nSupervisor running. Enroll accounts with `codex-switcher account add <name>`.\nNative switching remains blocked; inspect `codex-switcher doctor --probe`.",
                paths.root.display()
            );
        }
        Commands::Daemon {
            command: DaemonCommand::Serve,
        } => daemon::serve(paths).await?,
        Commands::Daemon {
            command: DaemonCommand::Start,
        } => {
            daemon::ensure_started(&paths).await?;
            println!("Supervisor running.");
        }
        Commands::Daemon {
            command: DaemonCommand::Stop,
        } => {
            print_json(&daemon::call(&paths, "daemon/stop", json!({})).await?)?;
        }
        Commands::Status { json: as_json } => {
            let value = match daemon::call(&paths, "status", json!({})).await {
                Ok(v) => v,
                Err(_) => {
                    json!({"schema_version":1,"daemon":{"running":false},"capabilities":{"managed_launch":false,"automatic_switching":false,"conversation_import":false},"readiness":{"ready":false,"code":"DAEMON_UNAVAILABLE"},"accounts":[],"sessions":[]})
                }
            };
            if as_json {
                print_json(&value)?;
            } else {
                println!(
                    "Supervisor: {}\nNative switching: unavailable (run doctor --probe)\nAccounts: {}",
                    if value["daemon"]["running"] == true {
                        "running"
                    } else {
                        "not running"
                    },
                    value["accounts"].as_array().map_or(0, Vec::len)
                );
            }
        }
        Commands::Ready => compatibility::require_native_handoff()?,
        Commands::AutoSwitch {
            thread,
            accounts,
            current_account,
            enable,
            disable,
            accept_experimental,
        } => {
            uuid::Uuid::parse_str(&thread)?;
            daemon::ensure_started(&paths).await?;
            if disable {
                daemon::call(&paths, "automatic/disable", json!({"thread":thread})).await?;
            }
            if let Some(accounts) = accounts {
                let accounts: Vec<_> = accounts
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                daemon::call(
                    &paths,
                    "automatic/set",
                    json!({"thread":thread,"accounts":accounts}),
                )
                .await?;
            }
            if enable {
                daemon::call(
                    &paths,
                    "automatic/enable",
                    json!({"thread":thread,"accept_experimental":accept_experimental}),
                )
                .await?;
            }
            print_json(
                &daemon::call(
                    &paths,
                    "automatic/get",
                    json!({"thread":thread,"current_account":current_account}),
                )
                .await?,
            )?;
        }
        Commands::AutoStart {
            thread,
            enable,
            disable,
            accept_experimental,
        } => {
            uuid::Uuid::parse_str(&thread)?;
            daemon::ensure_started(&paths).await?;
            let method = if disable {
                "auto-start/disable"
            } else if enable {
                "auto-start/enable"
            } else {
                "auto-start/get"
            };
            print_json(
                &daemon::call(
                    &paths,
                    method,
                    json!({"thread":thread,"accept_experimental":accept_experimental}),
                )
                .await?,
            )?;
        }
        Commands::Thresholds {
            account,
            short,
            weekly,
            multiplier,
        } => {
            daemon::ensure_started(&paths).await?;
            let mut params = json!({"name":account});
            if let Some(short) = short {
                params["short"] = json!(short);
            }
            if let Some(weekly) = weekly {
                params["weekly"] = json!(weekly);
            }
            if let Some(multiplier) = multiplier {
                params["multiplier"] = json!(multiplier);
            }
            print_json(&daemon::call(&paths, "account/thresholds", params).await?)?;
        }
        Commands::Account { command } => {
            daemon::ensure_started(&paths).await?;
            let (method, name) = match command {
                AccountCommand::Add { name } => ("account/add", Some(name)),
                AccountCommand::Login { name } => ("account/login", Some(name)),
                AccountCommand::Enable { name } => ("account/enable", Some(name)),
                AccountCommand::Disable { name } => ("account/disable", Some(name)),
                AccountCommand::List => ("status", None),
                AccountCommand::Show { .. } | AccountCommand::History { .. } => {
                    unreachable!("read-only account views handled above")
                }
            };
            let value = daemon::call(&paths, method, json!({"name":name})).await?;
            if method == "account/add" || method == "account/login" {
                println!(
                    "Open {}\nEnter code: {}\nAccount monitoring will begin automatically after login.\nCheck: codex-switcher account list",
                    value["login"]["verificationUrl"]
                        .as_str()
                        .unwrap_or("the verification URL returned by Codex"),
                    value["login"]["userCode"]
                        .as_str()
                        .unwrap_or("(no code returned)")
                );
            } else if method == "status" {
                print_json(&value["accounts"])?;
            } else {
                print_json(&value)?;
            }
        }
        Commands::Config { command } => {
            daemon::ensure_started(&paths).await?;
            let mut config = daemon::call(&paths, "config/get", json!({})).await?;
            if let Some(ConfigCommand::Set { key, value }) = command {
                anyhow::ensure!(config.get(&key).is_some(), "unknown setting {key}");
                config[&key] = serde_json::from_str(&value).unwrap_or(Value::String(value));
                let validated: Config = serde_json::from_value(config.clone())?;
                validated.validate()?;
                print_json(&daemon::call(&paths, "config/set", config).await?)?;
            } else {
                print_json(&config)?;
            }
        }
        Commands::Doctor {
            probe,
            json: as_json,
        } => {
            let config = store::load_config(&paths)?;
            let report = if probe {
                compatibility::probe(&config.codex_binary, &paths).await?
            } else {
                compatibility::Report::unverified(
                    compatibility::version(&config.codex_binary)
                        .await
                        .unwrap_or_else(|_| "unavailable".into()),
                )
            };
            if probe {
                compatibility::save_report(&paths.root.join("compatibility-report.json"), &report)?;
            }
            if as_json {
                print_json(&serde_json::to_value(report)?)?;
            } else {
                println!("{}\nAutomatic switching ready: NO", report.codex_version);
                for check in report.checks {
                    println!("{:?}: {} — {}", check.outcome, check.name, check.detail);
                }
            }
        }
        Commands::Instances => {
            print_json(&serde_json::to_value(external::discover(&paths).await?)?)?
        }
        Commands::Archives => print_json(&external::opening::archives_snapshot(&paths)?)?,
        Commands::Open { session, name } => {
            print_json(&external::opening::open(&paths, &session, name.as_deref()).await?)?
        }
        Commands::Rename { session, name } => {
            print_json(&external::catalog::rename(&paths, &session, &name).await?)?
        }
        Commands::DeleteSession {
            session,
            confirm,
            revision,
        } => {
            if let Some(confirm) = confirm {
                print_json(
                    &external::deletion::delete(
                        &paths,
                        &session,
                        &confirm,
                        revision.as_deref().unwrap_or_default(),
                    )
                    .await?,
                )?;
            } else {
                print_json(&serde_json::to_value(
                    external::deletion::preview(&paths, &session).await?,
                )?)?;
            }
        }
        Commands::Sessions {
            include_tmux,
            all,
            watch,
            interval,
        } => loop {
            let snapshot = if include_tmux {
                external::tmux_inventory::snapshot_with_tmux(&paths, all).await?
            } else {
                external::sessions_snapshot(&paths, all).await?
            };
            if !watch {
                print_json(&snapshot)?;
                break;
            }
            use std::io::Write;
            writeln!(std::io::stdout(), "{}", serde_json::to_string(&snapshot)?)?;
            std::io::stdout().flush()?;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
            }
        },
        Commands::Moves => print_json(&serde_json::to_value(external::jobs(&paths)?)?)?,
        Commands::Move {
            pane,
            thread,
            to,
            pause,
            accept_risks,
        } => {
            daemon::ensure_started(&paths).await?;
            print_json(&serde_json::to_value(
                external::queue_move(
                    &paths,
                    pane.as_deref(),
                    thread.as_deref(),
                    &to,
                    external::MoveOptions {
                        pause,
                        accept_risks,
                    },
                )
                .await?,
            )?)?;
        }
        Commands::MoveResponse {
            job,
            answer,
            sequence,
        } => {
            external::respond_move_sequence(&paths, &job, answer == "yes", sequence)?;
            print_json(&json!({"job":job,"response_recorded":true}))?;
        }
        Commands::CancelMove { job } => {
            external::cancel_move(&paths, &job)?;
            print_json(&json!({"job":job,"cancellation_requested":true}))?;
        }
        Commands::Restart {
            pane,
            pause,
            accept_risks,
        } => {
            daemon::ensure_started(&paths).await?;
            print_json(&serde_json::to_value(
                external::relocation::queue_restart(
                    &paths,
                    &pane,
                    external::MoveOptions {
                        pause,
                        accept_risks,
                    },
                )
                .await?,
            )?)?;
        }
        Commands::PauseSession {
            pane,
            pause,
            accept_risks,
        } => {
            daemon::ensure_started(&paths).await?;
            print_json(&serde_json::to_value(
                external::session_control::queue_pause(
                    &paths,
                    &pane,
                    external::MoveOptions {
                        pause,
                        accept_risks,
                    },
                )
                .await?,
            )?)?;
        }
        Commands::Terminate {
            pane,
            pause,
            accept_risks,
        } => {
            daemon::ensure_started(&paths).await?;
            print_json(&serde_json::to_value(
                external::session_control::queue_termination(
                    &paths,
                    &pane,
                    external::MoveOptions {
                        pause,
                        accept_risks,
                    },
                )
                .await?,
            )?)?;
        }
        Commands::Relocate {
            pane,
            thread,
            to,
            move_files,
            dry_run,
            pause,
            accept_risks,
        } => {
            if dry_run {
                print_json(
                    &external::relocation::preview(
                        &paths,
                        pane.as_deref(),
                        thread.as_deref(),
                        &to,
                        move_files,
                    )
                    .await?,
                )?;
            } else {
                daemon::ensure_started(&paths).await?;
                print_json(&serde_json::to_value(
                    external::relocation::queue(
                        &paths,
                        pane.as_deref(),
                        thread.as_deref(),
                        &to,
                        move_files,
                        external::MoveOptions {
                            pause,
                            accept_risks,
                        },
                    )
                    .await?,
                )?)?;
            }
        }
        Commands::BackgroundResponse { job, answer } => {
            external::respond_background(&paths, &job, answer == "yes")?;
            println!("Response recorded.");
        }
        Commands::Recover { job } => print_json(&serde_json::to_value(
            external::recover(&paths, &job).await?,
        )?)?,
        Commands::Retry { job } => {
            print_json(&serde_json::to_value(external::retry(&paths, &job).await?)?)?
        }
        Commands::HandoffWorker { job } => external::worker(&paths, &job).await?,
        Commands::Run {
            account,
            thread,
            session,
            codex_options,
        } => {
            if let Some(account) = account {
                anyhow::ensure!(
                    session.is_none(),
                    "--session is reserved for managed launching"
                );
                external::run_account_thread(&paths, &account, thread.as_deref(), &codex_options)
                    .await?;
            } else {
                compatibility::require_native_handoff()?;
            }
        }
        Commands::Import { .. } | Commands::Pause { .. } | Commands::Resume { .. } => {
            compatibility::require_native_handoff()?
        }
        Commands::Simulate { .. } => unreachable!(),
    }
    Ok(())
}
fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    #[serde(default)]
    config: Config,
    state: State,
    events: Vec<TimedInput>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimedInput {
    at: i64,
    input: Input,
}
fn simulate(path: &std::path::Path) -> Result<()> {
    let mut scenario: Scenario = serde_json::from_slice(&std::fs::read(path)?)?;
    scenario.config.validate()?;
    let mut last = i64::MIN;
    for event in scenario.events {
        anyhow::ensure!(event.at >= last, "scenario clock moved backwards");
        last = event.at;
        let effects =
            simulation::step(&mut scenario.state, &scenario.config, event.at, event.input)?;
        println!(
            "{}",
            serde_json::to_string(
                &json!({"at":event.at,"effects":effects,"sessions":scenario.state.sessions})
            )?
        );
    }
    Ok(())
}
fn install_systemd(paths: &Paths) -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME missing")?;
    let executable = std::env::current_exe()?;
    // systemd does not use shell quoting; quote specifiers and backslashes explicitly.
    let quote = |path: &std::path::Path| -> Result<String> {
        let s = path.to_str().context("systemd paths must be UTF-8")?;
        if s.chars().any(|c| c.is_control()) {
            bail!("control character in systemd path");
        }
        Ok(format!(
            "\"{}\"",
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$")
        ))
    };
    let service = format!(
        "[Unit]\nDescription=Codex Switcher account monitor\n\n[Service]\nType=simple\nExecStart={} --data-dir {} daemon serve\nRestart=on-failure\nRestartSec=5\nUMask=0077\n\n[Install]\nWantedBy=default.target\n",
        quote(&executable)?,
        quote(&paths.root)?
    );
    let path = PathBuf::from(home).join(".config/systemd/user/codex-switcher.service");
    atomic_write(&path, service.as_bytes())?;
    println!(
        "Wrote {}. Enable with: systemctl --user daemon-reload && systemctl --user enable --now codex-switcher",
        path.display()
    );
    Ok(())
}
