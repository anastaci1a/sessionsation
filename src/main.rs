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
    provider::{self, AccountRef, ProviderId},
    simulation::{self, Input},
    store,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "CLI-first account and session manager",
    long_about = "Account-aware Codex, Claude Code and Gemini CLI sessions in tmux and HERDR. Use api for the versioned JSON contract."
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
    #[command(hide = true)]
    AccountDeleteWorker { request_id: String },
    #[command(hide = true)]
    ConversationWorker { request_id: String },
    #[command(hide = true)]
    LaunchWorker { request_id: String },
    #[command(hide = true)]
    LaunchRun { request_id: String },
    #[command(hide = true)]
    ResumeWorker { request_id: String },
    #[command(hide = true)]
    SpaceMoveWorker { request_id: String },
    #[command(hide = true)]
    ResumeRun { request_id: String },
    /// Pane step of `api session add-to-herdr`: record this HERDR pane, then exec
    /// `claude attach` for the running background session.
    #[command(hide = true)]
    ClaudeAttach {
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    #[command(hide = true)]
    MessageRecoveryWorker { message_id: String },
    #[command(hide = true)]
    MessageRecoveryRun { message_id: String },
    #[command(hide = true)]
    ClaudeDeliveryWorker { message_id: String },
    /// Provider integration hooks (invoked by provider processes, not people).
    #[command(hide = true)]
    ProviderHook {
        #[command(subcommand)]
        command: ProviderHookCommand,
    },
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
    /// Run a deterministic offline scenario. Never starts a provider or reads credentials.
    #[command(hide = true)]
    Simulate { scenario: PathBuf },
    /// Start a session with a selected enrolled account (Codex, Claude Code or Gemini CLI); managed switching remains gated.
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
enum ProviderHookCommand {
    /// Record Claude statusline rate limits, then chain the account's own statusline.
    ClaudeStatusline {
        /// Record only; do not run the account's statusLine command.
        #[arg(long)]
        record_only: bool,
    },
}
#[derive(Subcommand)]
enum AccountCommand {
    /// Enroll an account. Names are unique across providers.
    #[command(
        after_help = "Examples:\n  sessionsation account add --platform codex work\n  sessionsation account add --platform claude personal\n  sessionsation account add --platform claude --config-dir /home/me/.claude existing\n  sessionsation account add --platform gemini me"
    )]
    Add {
        /// Provider for the new account (required; no default).
        #[arg(long, value_enum, value_name = "codex|claude|gemini")]
        platform: ProviderId,
        /// Claude only: adopt an existing CLAUDE_CONFIG_DIR in place (absolute; never copied or moved).
        #[arg(long, value_name = "DIR")]
        config_dir: Option<PathBuf>,
        /// Claude and Gemini: do not start the interactive login after enrolling a fresh managed home.
        #[arg(long)]
        no_login: bool,
        name: String,
    },
    /// Log in: Codex prints a device code; Claude runs `claude auth login --claudeai` here; Gemini runs `gemini` with NO_BROWSER=true here (choose Login with Google).
    Login {
        /// NAME or PROVIDER:NAME.
        name: String,
    },
    /// Set a display name (the stable name, id and home never change); --clear resets it.
    Rename {
        /// NAME, PROVIDER:NAME or an unambiguous display name.
        name: String,
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        display_name: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Claude: switch an adopted account to its managed home <root>/claude-accounts/NAME.
    /// The directory must already exist (0700, yours, no symlink) and hold the same login;
    /// nothing is copied. Refused while Claude is running on either home.
    ManagedHome {
        /// NAME or claude:NAME.
        name: String,
    },
    /// List accounts of every provider (table; --json for machine output).
    List {
        #[arg(long, value_enum, value_name = "codex|claude|gemini")]
        platform: Option<ProviderId>,
        #[arg(long)]
        json: bool,
    },
    Enable {
        /// NAME or PROVIDER:NAME.
        name: String,
    },
    Disable {
        /// NAME or PROVIDER:NAME.
        name: String,
    },
    /// Read account details, plan, limits and remaining percentages without starting a monitor.
    Show { name: String },
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
            json!({"api_version":1,"ok":false,"error":{"code":error_code(&error),"native":error.downcast_ref::<sessionsation::messaging::NativeReadFailure>(),"message":sessionsation::messaging::recovery::diagnostic(&error_code(&error)).map(|help|format!("{}: {}",error_code(&error),help)).unwrap_or_else(||format!("{error:#}")),"retryable":matches!(error_code(&error).as_str(),"ADAPTER_BUSY"|"OPERATION_BUSY"|"OBSERVATION_TIMEOUT"|"ATTACHMENT_UNOBSERVED"|"SESSION_OPERATION_IN_PROGRESS")}})
        );
        std::process::exit(1);
    }
}
async fn run(cli: Cli) -> Result<()> {
    // Offline simulation deliberately avoids creating a daemon or accessing user state.
    if let Commands::Simulate { scenario } = &cli.command {
        return simulate(scenario);
    }
    // Runs inside Claude's statusline: never create or touch a data root.
    if let Commands::ProviderHook {
        command: ProviderHookCommand::ClaudeStatusline { record_only },
    } = &cli.command
    {
        std::process::exit(provider::claude::statusline_hook(*record_only));
    }
    // Only setup may create a data root; everything else fails closed.
    let paths = if matches!(cli.command, Commands::Setup { .. }) {
        Paths::new(cli.data_dir)?
    } else {
        Paths::open(cli.data_dir)?
    };
    match cli.command {
        Commands::ConversationWorker { request_id } => {
            sessionsation::external::archiving::worker(&paths, &request_id).await?;
        }
        Commands::ClaudeDeliveryWorker { message_id } => {
            sessionsation::messaging::headless::worker(&paths, &message_id).await?
        }
        Commands::AccountDeleteWorker { request_id } => {
            sessionsation::account_deletion::worker(&paths, &request_id).await?
        }
        Commands::MessageRecoveryWorker { message_id } => {
            sessionsation::messaging::recovery::worker(&paths, &message_id).await?
        }
        Commands::MessageRecoveryRun { message_id } => {
            sessionsation::messaging::recovery::run(&paths, &message_id).await?
        }
        Commands::SpaceMoveWorker { request_id } => {
            sessionsation::external::spaces::worker(&paths, &request_id)?
        }
        Commands::ResumeWorker { request_id } => {
            sessionsation::resume::worker(&paths, &request_id).await?
        }
        Commands::ClaudeAttach {
            session,
            expected_generation,
        } => sessionsation::provider::claude::attach::exec_attach(
            &paths,
            &session,
            &expected_generation,
        )?,
        Commands::ResumeRun { request_id } => {
            sessionsation::resume::run(&paths, &request_id).await?
        }
        Commands::LaunchWorker { request_id } => {
            sessionsation::launch::worker(&paths, &request_id).await?
        }
        Commands::LaunchRun { request_id } => {
            sessionsation::launch::run(&paths, &request_id).await?
        }
        Commands::Api { command, .. } => sessionsation::api::run(&paths, command).await?,
        Commands::Session { session } => {
            print_json(&sessionsation::cli_views::session(&paths, &session).await?)?;
        }
        Commands::Account {
            command: AccountCommand::Show { name },
        } => {
            let account = resolve_account(&paths, &name)?;
            let status = daemon::call(&paths, "status", json!({})).await?;
            print_json(&sessionsation::cli_views::account(&status, &account.name)?)?;
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
            let account = resolve_account(&paths, &name)?;
            let history = daemon::call(
                &paths,
                "account/history",
                json!({"name":account.name,"provider":account.provider,"start":start,"end":end,"step":step}),
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
                "Initialized {}\nSupervisor running. Enroll accounts with `sessionsation account add --platform codex|claude|gemini <name>`.\nNative switching remains blocked; inspect `sessionsation doctor --probe`.",
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
        Commands::Account {
            command: AccountCommand::Login { name },
        } if provider::native::is_native(resolve_account(&paths, &name)?.provider) => {
            // Interactive official login in this terminal, with the account's
            // isolated environment. Credentials never pass through us.
            let resolved = resolve_account(&paths, &name)?;
            let account = provider::native::account(&paths, resolved.provider, &resolved.name)?;
            let config = store::load_config(&paths)?;
            return Err(exec_login(&config, resolved.provider, &account.home));
        }
        Commands::Account {
            command:
                AccountCommand::Add {
                    platform: platform @ (ProviderId::Claude | ProviderId::Gemini),
                    config_dir,
                    no_login,
                    name,
                },
        } => {
            anyhow::ensure!(
                config_dir.is_none() || platform == ProviderId::Claude,
                "INVALID_ARGUMENT: --config-dir is only supported with --platform claude"
            );
            daemon::ensure_started(&paths).await?;
            let adopted = config_dir.is_some();
            let mut params = json!({"name":name,"provider":platform});
            if let Some(dir) = config_dir {
                params["config_dir"] = json!(dir);
            }
            let value = daemon::call(&paths, "account/add", params).await?;
            let auth = match value["auth"]["problem"].as_str() {
                None if platform == ProviderId::Gemini => {
                    "logged in (Login with Google)".to_owned()
                }
                None => "logged in (claude.ai subscription)".to_owned(),
                Some("AUTH_REQUIRED") => {
                    format!("not logged in; run: sessionsation account login {platform}:{name}")
                }
                Some(problem) => {
                    format!("{problem}; run: sessionsation account login {platform}:{name}")
                }
            };
            println!(
                "Enrolled {platform}:{name} ({} home {}).\nAuth: {auth}\nCheck: sessionsation account list",
                if value["existing_home"] == true {
                    "registered existing"
                } else {
                    "created"
                },
                value["home"].as_str().unwrap_or("unknown")
            );
            // A fresh managed home goes straight into the same interactive login
            // `account login` runs, when there is a terminal to run it in.
            use std::io::IsTerminal;
            if value["auth"]["problem"] == "AUTH_REQUIRED"
                && !adopted
                && value["existing_home"] != true
                && !no_login
                && std::io::stdin().is_terminal()
            {
                println!(
                    "Starting {} login for {platform}:{name}...",
                    provider::native::title(platform)
                );
                let account = provider::native::account(&paths, platform, &name)?;
                let config = store::load_config(&paths)?;
                return Err(exec_login(&config, platform, &account.home));
            }
        }
        Commands::Account {
            command:
                AccountCommand::Rename {
                    name,
                    display_name,
                    clear,
                },
        } => {
            let account = resolve_account(&paths, &name)?;
            daemon::ensure_started(&paths).await?;
            let value = daemon::call(
                &paths,
                "account/rename",
                json!({"name":account.name,"provider":account.provider,"display_name":if clear {None} else {display_name}}),
            )
            .await?;
            match value["display_name"].as_str() {
                Some(display) => println!("{} is now shown as {display}.", account.id()),
                None => println!("{} display name cleared.", account.id()),
            }
        }
        Commands::Account {
            command:
                AccountCommand::List {
                    platform,
                    json: as_json,
                },
        } => {
            daemon::ensure_started(&paths).await?;
            let value = daemon::call(&paths, "status", json!({})).await?;
            let accounts = sessionsation::api::project_accounts(
                &paths,
                value["accounts"]
                    .as_array()
                    .context("INVALID_SUPERVISOR_RESPONSE")?
                    .clone(),
            )?
            .into_iter()
            .filter(|a| platform.is_none_or(|p| a["provider"] == p.as_str()))
            .collect::<Vec<_>>();
            if as_json {
                print_json(&json!(accounts))?;
            } else {
                print!("{}", sessionsation::cli_views::account_table(&accounts));
            }
        }
        Commands::Account { command } => {
            daemon::ensure_started(&paths).await?;
            let (method, name, config_dir) = match command {
                AccountCommand::Add {
                    platform: ProviderId::Codex,
                    config_dir,
                    name,
                    ..
                } => (
                    "account/add",
                    AccountRef {
                        provider: ProviderId::Codex,
                        name,
                    },
                    config_dir,
                ),
                AccountCommand::Login { name } => {
                    ("account/login", resolve_account(&paths, &name)?, None)
                }
                AccountCommand::ManagedHome { name } => (
                    "account/managed-home",
                    resolve_account(&paths, &name)?,
                    None,
                ),
                AccountCommand::Enable { name } => {
                    ("account/enable", resolve_account(&paths, &name)?, None)
                }
                AccountCommand::Disable { name } => {
                    ("account/disable", resolve_account(&paths, &name)?, None)
                }
                AccountCommand::Add { .. }
                | AccountCommand::List { .. }
                | AccountCommand::Rename { .. }
                | AccountCommand::Show { .. }
                | AccountCommand::History { .. } => {
                    unreachable!("handled above")
                }
            };
            anyhow::ensure!(
                config_dir.is_none(),
                "INVALID_ARGUMENT: --config-dir is only supported with --platform claude"
            );
            sessionsation::accounts::ensure_available(&paths, &name.name)?;
            let value = daemon::call(
                &paths,
                method,
                json!({"name":name.name,"provider":name.provider}),
            )
            .await?;
            if method == "account/managed-home" {
                print_json(&value)?;
            } else if method == "account/add" || method == "account/login" {
                println!(
                    "Open {}\nEnter code: {}\nAccount monitoring will begin automatically after login.\nCheck: sessionsation account list",
                    value["login"]["verificationUrl"]
                        .as_str()
                        .unwrap_or("the verification URL returned by Codex"),
                    value["login"]["userCode"]
                        .as_str()
                        .unwrap_or("(no code returned)")
                );
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
            // Claude sessions are addressed by stable catalog id and take the same
            // HERDR tab/catalog rename path as `api rename`.
            let claude = sessionsation::catalog::list(&paths)?
                .into_iter()
                .find(|s| s.id == session && provider::native::of(&s.provider).is_some());
            if let Some(s) = claude {
                let renamed =
                    sessionsation::catalog::names::rename(&paths, &s.id, &s.generation(), &name)
                        .await?;
                print_json(
                    &json!({"schema_version":1,"id":renamed.id,"thread_id":renamed.conversation_id,"name":renamed.name}),
                )?
            } else {
                print_json(&external::catalog::rename(&paths, &session, &name).await?)?
            }
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
                let account = resolve_account(&paths, &account)?;
                if provider::native::is_native(account.provider) && matches!(host, Host::Inline) {
                    anyhow::ensure!(
                        herdr_session.is_none(),
                        "--herdr-session requires --host herdr"
                    );
                    anyhow::ensure!(
                        codex_options.is_empty(),
                        "UNSUPPORTED_PROVIDER_OPERATION: provider options after -- are Codex-only"
                    );
                    if let Some(thread) = thread {
                        let s = sessionsation::catalog::list(&paths)?
                            .into_iter()
                            .find(|s| s.provider == account.provider.as_str() && s.conversation_id == thread && s.account == account.name)
                            .context("SESSION_NOT_FOUND: no cataloged session of this provider with this id on the account")?;
                        provider::native::exec_resume(
                            &paths,
                            account.provider,
                            &s.id,
                            &s.generation(),
                        )
                        .await?;
                    } else {
                        provider::native::exec_new(
                            &paths,
                            account.provider,
                            &account.name,
                            None,
                            None,
                        )
                        .await?;
                    }
                    return Ok(());
                }
                let account = account.name;
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
        Commands::Simulate { .. } | Commands::ProviderHook { .. } => unreachable!(),
    }
    Ok(())
}
/// Resolve NAME or PROVIDER:NAME against enrolled accounts (names are global).
/// An unknown bare name stays on the Codex path, whose launcher reports it.
/// Exec the official interactive login for a native provider in this terminal.
/// Returns only on failure to start it.
fn exec_login(config: &Config, provider: ProviderId, home: &std::path::Path) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    match provider {
        ProviderId::Gemini => {
            let mut command = provider::gemini::login_command(config, home);
            command.current_dir(provider::gemini::login_directory(home));
            anyhow::Error::new(command.exec()).context("could not start gemini login")
        }
        _ => {
            let mut command = provider::claude::command(config, home);
            command
                .args(&provider::claude::login_argv(config)[1..])
                .current_dir(home);
            anyhow::Error::new(command.exec()).context("could not start claude auth login")
        }
    }
}
fn resolve_account(paths: &Paths, id: &str) -> Result<AccountRef> {
    let (wanted, name) = AccountRef::parse_lookup(id)?;
    let state = if paths.root.join("registry.db").exists() {
        store::load_state_snapshot(paths).ok()
    } else {
        None
    };
    if let Some(state) = &state
        && state.accounts.contains_key(&name)
    {
        return provider::resolve(
            state
                .accounts
                .values()
                .map(|a| (a.name.as_str(), a.provider)),
            id,
        );
    }
    // Human commands may address an account by display name when unambiguous.
    if let Some(state) = &state {
        let matches: Vec<_> = state
            .accounts
            .values()
            .filter(|a| a.display_name.as_deref() == Some(name.as_str()))
            .filter(|a| wanted.is_none_or(|p| p == a.provider))
            .collect();
        anyhow::ensure!(
            matches.len() <= 1,
            "ACCOUNT_AMBIGUOUS: display name '{name}' matches several accounts; use PROVIDER:NAME"
        );
        if let Some(a) = matches.first() {
            return Ok(AccountRef {
                provider: a.provider,
                name: a.name.clone(),
            });
        }
    }
    Ok(AccountRef {
        provider: wanted.unwrap_or(ProviderId::Codex),
        name,
    })
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
        "[Unit]\nDescription=Sessionsation account monitor\n\n[Service]\nType=simple\nExecStart={} --data-dir {} daemon serve\nRestart=on-failure\nRestartSec=5\nUMask=0077\n\n[Install]\nWantedBy=default.target\n",
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
    if error
        .downcast_ref::<sessionsation::messaging::NativeReadFailure>()
        .is_some()
    {
        return "NATIVE_REJECTED".into();
    }
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
