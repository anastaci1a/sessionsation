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
    about = "CLI-first account and session manager",
    long_about = "Account-aware Codex sessions in tmux and HERDR. Use api for the versioned JSON contract. Claude runtime support requires separate setup."
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
    /// Versioned machine-readable backend contract (JSON/NDJSON).
    Api {
        #[arg(long, default_value_t=1, value_parser=clap::value_parser!(u32).range(1..=1))]
        api_version: u32,
        #[command(subcommand)]
        command: sessionsation::api::Command,
    },
    /// Transport-neutral, attach-only messaging API (JSON).
    #[command(hide = true)]
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
    #[command(hide = true)]
    Ready,
    /// List live tmux instances and completed inactive conversations available to move.
    #[command(hide = true)]
    Instances,
    /// Inspect one exact inventory ID, its account usage and automation settings (JSON).
    #[command(hide = true)]
    Session {
        #[arg(long)]
        session: String,
    },
    /// List archived conversations available for explicit restoration (JSON).
    #[command(hide = true)]
    Archives,
    /// Open a live or saved inventory ID in tmux, without sending a prompt (JSON).
    #[command(hide = true)]
    Open {
        #[arg(long)]
        session: String,
        /// Name for a newly created tmux session; existing live sessions are reused.
        #[arg(long)]
        name: Option<String>,
    },
    /// Rename a live or saved conversation; the name survives moves and reopening.
    #[command(hide = true)]
    Rename {
        #[arg(long)]
        session: String,
        #[arg(long)]
        name: String,
    },
    /// Preview permanent deletion of an inactive conversation and its local history copies.
    #[command(hide = true)]
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
    #[command(hide = true)]
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
    #[command(hide = true)]
    Moves,
    /// Move a live pane or completed inactive thread to another account.
    #[command(hide = true)]
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
    #[command(hide = true)]
    MoveResponse {
        job: String,
        #[arg(value_parser = ["yes", "no"])]
        answer: String,
        /// Reject an answer to a dialog that has changed since it was displayed.
        #[arg(long)]
        sequence: Option<u64>,
    },
    /// Cancel a queued move before the source restart begins.
    #[command(hide = true)]
    CancelMove { job: String },
    /// Restart a live Codex terminal using the installed binary and its exact conversation.
    #[command(hide = true)]
    Restart {
        #[arg(long)]
        pane: String,
        #[arg(long)]
        pause: bool,
        #[arg(long)]
        accept_risks: bool,
    },
    /// Pause a live session and clean up approved background work, leaving Codex open.
    #[command(hide = true)]
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
    #[command(hide = true)]
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
    #[command(hide = true)]
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
    #[command(hide = true)]
    BackgroundResponse {
        job: String,
        #[arg(value_parser = ["yes", "no"])]
        answer: String,
    },
    /// Relaunch a failed move from the original account home.
    #[command(hide = true)]
    Recover { job: String },
    /// Retry a stopped job with fresh session discovery and new confirmations.
    #[command(hide = true)]
    Retry { job: String },
    #[command(hide = true)]
    HandoffWorker {
        #[arg(long)]
        job: String,
    },
    #[command(hide = true)]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Configure experimental automatic moves, or disable them immediately.
    #[command(hide = true)]
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
    #[command(hide = true)]
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
    #[command(hide = true)]
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
    #[command(hide = true)]
    Simulate { scenario: PathBuf },
    /// Start Codex with a selected enrolled account; managed switching remains gated.
    Run {
        /// Terminal hosting; inline retains the invoking terminal.
        #[arg(long, value_enum, default_value = "inline")]
        host: Host,
        /// Existing named headless HERDR server (required with --host herdr).
        #[arg(long)]
        herdr_session: Option<String>,
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
    #[command(hide = true)]
    Import {
        #[arg(long)]
        thread: String,
        #[arg(long)]
        from: PathBuf,
    },
    #[command(hide = true)]
    Pause { session: String },
    /// Resume a saved stable backend ID on its recorded account in this terminal.
    Resume {
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    /// Open a plain interactive shell; never re-enter the terminal launcher.
    Shell,
}
#[derive(Clone, Copy, clap::ValueEnum)]
enum Host {
    Inline,
    Herdr,
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
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(error) => {
            eprintln!(
                "{}",
                json!({"api_version":1,"ok":false,"error":{"code":"INVALID_ARGUMENT","message":error.to_string(),"retryable":false}})
            );
            std::process::exit(2);
        }
    };
    if let Err(error) = run(cli).await {
        eprintln!(
            "{}",
            json!({"api_version":1,"ok":false,"error":{"code":error_code(&error),"message":format!("{error:#}"),"retryable":false}})
        );
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
        Commands::Api { command, .. } => sessionsation::api::run(&paths, command).await?,
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
                "Initialized {}\nSupervisor running. Enroll accounts with `sessionsation account add <name>`.\nNative switching remains blocked; inspect `sessionsation doctor --probe`.",
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
            if let Some(name) = name.as_deref() {
                sessionsation::accounts::ensure_available(&paths, name)?;
            }
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
                print_json(&json!(sessionsation::api::project_accounts(
                    &paths,
                    value["accounts"]
                        .as_array()
                        .context("INVALID_SUPERVISOR_RESPONSE")?
                        .clone()
                )?))?;
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
            host,
            herdr_session,
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
                match host {
                    Host::Inline => {
                        anyhow::ensure!(
                            herdr_session.is_none(),
                            "--herdr-session requires --host herdr"
                        );
                        external::run_account_thread(
                            &paths,
                            &account,
                            thread.as_deref(),
                            &codex_options,
                        )
                        .await?;
                    }
                    Host::Herdr => print_json(
                        &external::hosting::launch(
                            &paths,
                            herdr_session
                                .as_deref()
                                .context("--host herdr requires --herdr-session")?,
                            &account,
                            thread.as_deref(),
                            &codex_options,
                        )
                        .await?,
                    )?,
                }
            } else {
                compatibility::require_native_handoff()?;
            }
        }
        Commands::Resume {
            session,
            expected_generation,
        } => sessionsation::api::resume_current(&paths, &session, &expected_generation).await?,
        Commands::Shell => {
            use std::os::unix::process::CommandExt;
            let shell =
                std::env::var("SESSIONSATION_PLAIN_SHELL").unwrap_or_else(|_| "/bin/bash".into());
            anyhow::ensure!(std::path::Path::new(&shell).is_absolute(), "INVALID_SHELL");
            return Err(std::process::Command::new(shell)
                .arg("-i")
                .env("SESSIONSATION_LAUNCHER_BYPASS", "1")
                .exec())
            .context("could not open shell");
        }
        Commands::Import { .. } | Commands::Pause { .. } => {
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
    let path = PathBuf::from(home).join(".config/systemd/user/sessionsation.service");
    atomic_write(&path, service.as_bytes())?;
    println!(
        "Wrote {}. Enable with: systemctl --user daemon-reload && systemctl --user enable --now sessionsation",
        path.display()
    );
    Ok(())
}

fn error_code(error: &anyhow::Error) -> String {
    if let Some(fault) = error.downcast_ref::<sessionsation::messaging::Fault>() {
        return fault.0.to_ascii_uppercase();
    }
    let text = error.to_string();
    let candidate = text.split(':').next().unwrap_or_default();
    if !candidate.is_empty()
        && candidate.len() <= 64
        && candidate
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        candidate.into()
    } else {
        "OPERATION_FAILED".into()
    }
}
