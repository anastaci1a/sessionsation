#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests are exempt from owned roles"
    )
)]
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use sessionsation::{
    config::Config,
    daemon, external,
    paths::{Paths, atomic_write, private_dir},
    provider::{
        self, AccountRef, ProviderId,
        ops::{self, Enrollment},
    },
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
    #[arg(
        long,
        global = true,
        value_enum,
        help = "Permissions for provider processes this command starts: yolo (full access, no prompts) | default (provider prompts); overrides the launch_permissions setting"
    )]
    permissions: Option<sessionsation::config::LaunchPermissions>,
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
    ClaudeSwitchWorker { request_id: String },
    /// One auto-recover pass (the supervisor runs this loop itself); prints the recoveries it ran.
    #[command(hide = true)]
    AutoRecoverTick,
    /// Rewrite the Active `state --cached` snapshot (spawned detached after lifecycle calls).
    #[command(hide = true)]
    SnapshotRefresh,
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
    #[command(hide = true)]
    ClaudeHostWorker { conversation_id: String },
    #[command(hide = true)]
    ClaudePeerWorker { message_id: String },
    #[command(hide = true)]
    ClaudeIdleWorker { id: String },
    #[command(hide = true)]
    ClaudeTerminalWorker { message_id: String },
    #[command(hide = true)]
    AntigravityTerminalWorker { message_id: String },
    /// Contract 1.49.0: apply one Claude model change in its HERDR pane.
    #[command(hide = true)]
    ModelChangeWorker { id: String },
    /// Render a Sessionsation-hosted Claude session live (t + Enter takes it over).
    View {
        /// Catalog session id or hosted conversation id.
        session: String,
        /// Print the current log and exit.
        #[arg(long)]
        once: bool,
    },
    /// Stop a hosted Claude worker cleanly, then resume the conversation interactively here.
    TakeOver { session: String },
    /// Provider integration hooks (invoked by provider processes, not people).
    #[command(hide = true)]
    ProviderHook {
        #[command(subcommand)]
        command: ProviderHookCommand,
    },
    /// Versioned machine-readable backend contract (JSON/NDJSON).
    Api {
        #[arg(long, default_value = "1", value_parser=sessionsation::api::parse_api_version)]
        api_version: u32,
        #[command(subcommand)]
        command: sessionsation::api::Command,
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
    /// List live tmux instances and completed inactive conversations available to move.
    #[command(hide = true)]
    Instances,
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
        /// Keep the HERDR pane open after the session ends (default: close it once verified).
        #[arg(long)]
        keep_pane: bool,
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
    /// Rename a live or saved conversation; the name survives moves and reopening.
    #[command(hide = true)]
    Rename {
        #[arg(long)]
        session: String,
        #[arg(long)]
        name: String,
    },
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
        /// Claude: run a Sessionsation-hosted (stream-json) session and show its viewer here.
        #[arg(long)]
        hosted: bool,
        /// With --hosted: account (default) | default | acceptEdits | plan | bypassPermissions.
        #[arg(long, requires = "hosted")]
        permission_mode: Option<String>,
        /// Name of the new session (same rules as `api launch`); also labels this HERDR tab. Inline host only.
        #[arg(long)]
        name: Option<String>,
        #[arg(last = true)]
        codex_options: Vec<String>,
    },
    /// Resume a saved stable backend ID on its recorded account in this terminal.
    Resume {
        session: String,
        #[arg(long)]
        expected_generation: String,
        /// Acknowledge only the unfinished-history warning of a saved Codex session.
        #[arg(long)]
        confirm_unfinished: bool,
        /// Rename the session first (same rules as `api resume --name`); also labels this HERDR tab.
        #[arg(long)]
        name: Option<String>,
    },
    /// Open a plain interactive shell; never re-enter the terminal launcher.
    Shell,
    /// Data-root layout: report its version, deferred homes and legacy entries.
    Layout {
        #[command(subcommand)]
        command: LayoutCommand,
    },
    /// Retention for logs, cache, operations history and receipts.
    Retention {
        #[command(subcommand)]
        command: RetentionCommand,
    },
    /// Shared Claude conversation state (projects, file-history, todos, session-env) in one store for every account.
    Shared {
        #[command(subcommand)]
        command: SharedCommand,
    },
}
#[derive(Subcommand)]
enum SharedCommand {
    /// Store state and each Claude account's link state.
    Status,
    /// Turn the feature on for this root (creates the store; new and empty homes are then linked).
    Enable,
}
#[derive(Subcommand)]
enum LayoutCommand {
    /// Layout version, deferred account homes (with reasons) and legacy entries.
    Status,
}
#[derive(Subcommand)]
enum RetentionCommand {
    /// Run one retention pass now.
    Run {
        /// List what would be removed; receipts are skipped.
        #[arg(long)]
        dry_run: bool,
    },
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
    /// Record one Claude Code hook event (content-free fields only) for `api events`.
    ClaudeEvent,
}
#[derive(Subcommand)]
enum AccountCommand {
    /// Enroll an account. Names are unique across providers.
    #[command(
        after_help = "Examples:\n  sessionsation account add --platform codex work\n  sessionsation account add --platform claude personal\n  sessionsation account add --platform claude --config-dir /home/me/.claude existing\n  sessionsation account add --platform gemini me\n  sessionsation account add --platform antigravity me"
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
    /// Log in: Codex prints a device code; Claude runs `claude auth login --claudeai` here.
    Login {
        /// NAME or PROVIDER:NAME.
        name: String,
    },
    /// Move an account to a 0-based position in the shared account order (list and UI).
    Reorder {
        /// NAME or PROVIDER:NAME.
        name: String,
        #[arg(long)]
        position: u32,
    },
    /// Sign out: Codex app-server account/logout (or `codex logout`), `claude auth logout`, or Gemini's own credential clearing. Refused while the account is in use; log in again afterwards.
    Logout {
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
    /// Claude: let live sessions of this account accept Sessionsation steering as peer
    /// messages (sets `crossSessionInbound: accept` in its settings.json), or stop.
    /// Gemini: turn Gemini CLI model steering on or off (sets or removes
    /// `experimental.modelSteering` in GEMINI_CLI_HOME/.gemini/settings.json); running
    /// sessions keep their value until restarted. Without a flag, shows the current setting.
    LiveSteering {
        /// NAME, claude:NAME or gemini:NAME.
        name: String,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
    /// Claude: skip or show Claude Code's bypass-permissions (YOLO) startup warning
    /// (sets `skipDangerousModePermissionPrompt` in the account's settings.json).
    /// Without a flag, shows the current setting.
    BypassWarning {
        /// claude:NAME.
        name: String,
        #[arg(long, conflicts_with = "show")]
        skip: bool,
        #[arg(long)]
        show: bool,
    },
    /// Claude: switch an adopted account to its managed home <root>/accounts/claude/NAME.
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
}
#[derive(Subcommand)]
enum ConfigCommand {
    Get,
    Set { key: String, value: String },
}

fn main() {
    // Before the runtime starts a thread: no provider, worker or pane command we start resolves
    // tools through the FUSE share (contract 1.67.0).
    sessionsation::alias::normalize_process_path();
    run_main();
}
#[tokio::main]
async fn run_main() {
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
        eprintln!("{}", error_envelope(&error));
        std::process::exit(1);
    }
}
async fn run(cli: Cli) -> Result<()> {
    if let Some(mode) = cli.permissions {
        sessionsation::config::override_launch_permissions(mode);
    }
    // Runs inside Claude's statusline: never create or touch a data root.
    if let Commands::ProviderHook {
        command: ProviderHookCommand::ClaudeStatusline { record_only },
    } = &cli.command
    {
        std::process::exit(provider::claude::statusline_hook(*record_only));
    }
    if let Commands::ProviderHook {
        command: ProviderHookCommand::ClaudeEvent,
    } = &cli.command
    {
        std::process::exit(provider::claude::hooks::event_hook());
    }
    // Only setup may create a data root; everything else fails closed.
    let any_layout = matches!(
        cli.command,
        Commands::Layout { .. }
            | Commands::Api {
                command: sessionsation::api::Command::Version,
                ..
            }
    );
    let paths = if matches!(cli.command, Commands::Setup { .. }) {
        Paths::new(cli.data_dir)?
    } else if any_layout {
        Paths::open_any_layout(cli.data_dir)?
    } else {
        Paths::open(cli.data_dir)?
    };
    match cli.command {
        Commands::Layout { command } => {
            let LayoutCommand::Status = command;
            print_json(&sessionsation::layout::status(&paths.root)?)?;
        }
        Commands::Retention {
            command: RetentionCommand::Run { dry_run },
        } => print_json(&sessionsation::retention::run(&paths, dry_run)?)?,
        Commands::Shared { command } => {
            use sessionsation::shared_state as shared;
            print_json(&match command {
                SharedCommand::Status => shared::status(&paths)?,
                SharedCommand::Enable => {
                    shared::enable(&paths.root)?;
                    shared::status(&paths)?
                }
            })?;
        }
        Commands::ConversationWorker { request_id } => {
            sessionsation::external::archiving::worker(&paths, &request_id).await?;
        }
        Commands::ClaudeSwitchWorker { request_id } => {
            sessionsation::claude_switch::worker(&paths, &request_id).await?;
        }
        Commands::AutoRecoverTick => {
            let outcomes = sessionsation::auto_recover::tick(&paths).await?;
            println!("{}", serde_json::to_string(&outcomes)?);
        }
        Commands::ClaudeDeliveryWorker { message_id } => {
            sessionsation::messaging::headless::worker(&paths, &message_id).await?
        }
        Commands::ClaudeHostWorker { conversation_id } => {
            sessionsation::provider::claude::hosted::worker(&paths, &conversation_id).await?
        }
        Commands::ClaudeIdleWorker { id } => {
            sessionsation::claude_idle::worker(&paths, &id).await?
        }
        Commands::ClaudePeerWorker { message_id } => {
            sessionsation::messaging::claude_steer::peer_worker(&paths, &message_id).await?
        }
        Commands::ClaudeTerminalWorker { message_id } => {
            sessionsation::messaging::claude_steer::terminal::worker(&paths, &message_id).await?
        }
        Commands::AntigravityTerminalWorker { message_id } => {
            sessionsation::messaging::claude_steer::antigravity::worker(&paths, &message_id).await?
        }
        Commands::ModelChangeWorker { id } => {
            sessionsation::messaging::model_change::claude::worker(&paths, &id).await?
        }
        Commands::View { session, once } => {
            sessionsation::provider::claude::hosted::view(&paths, &session, once).await?
        }
        Commands::TakeOver { session } => {
            sessionsation::provider::claude::hosted::take_over(&paths, &session).await?
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
        Commands::SnapshotRefresh => sessionsation::api::refresh_active_snapshot(&paths).await,
        Commands::LaunchWorker { request_id } => {
            sessionsation::launch::worker(&paths, &request_id).await?
        }
        Commands::LaunchRun { request_id } => {
            // Contract 1.49.1: never return to the pane's shell silently.
            if let Err(error) = sessionsation::launch::run(&paths, &request_id).await {
                eprintln!("{}", sessionsation::launch::failure_line(&error));
                std::process::exit(1);
            }
        }
        Commands::Api { command, .. } => sessionsation::api::run(&paths, command).await?,
        Commands::Account {
            command: AccountCommand::Reorder { name, position },
        } => {
            let account = resolve_account(&paths, &name)?;
            daemon::ensure_started(&paths).await?;
            let value = daemon::call(
                &paths,
                "account/reorder",
                json!({"name":account.name,"provider":account.provider,"position":position}),
            )
            .await?;
            print_json(&value)?;
        }
        Commands::Account {
            command:
                AccountCommand::LiveSteering {
                    name,
                    enable,
                    disable,
                },
        } => {
            let account = resolve_account(&paths, &name)?;
            daemon::ensure_started(&paths).await?;
            let value = daemon::call(
                &paths,
                "account/live-steering",
                json!({"name":account.name,"provider":account.provider,"enable":if enable {Some(true)} else if disable {Some(false)} else {None}}),
            )
            .await?;
            print_json(&value)?;
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
                "Initialized {}\nSupervisor running. Enroll accounts with `sessionsation account add --platform codex|claude|gemini|antigravity <name>`.\nNative switching is not available.",
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
                    "Supervisor: {}\nNative switching: unavailable\nAccounts: {}",
                    if value["daemon"]["running"] == true {
                        "running"
                    } else {
                        "not running"
                    },
                    value["accounts"].as_array().map_or(0, Vec::len)
                );
            }
        }
        Commands::Account {
            command: AccountCommand::Login { name },
        } if provider::native::is_native(resolve_account(&paths, &name)?.provider) => {
            // Interactive official login in this terminal, with the account's
            // isolated environment. Credentials never pass through us.
            let resolved = resolve_account(&paths, &name)?;
            let account = provider::native::account(&paths, resolved.provider, &resolved.name)?;
            let config = store::load_config(&paths)?;
            if matches!(
                ops::facts(resolved.provider).enrollment,
                Enrollment::ClaudeHome
            ) && sessionsation::layout::same_home(
                &account.home,
                &provider::claude::default_home(&paths, &resolved.name)?,
            ) && !provider::claude::peer::settings_file(&account.home).exists()
            {
                // A brand-new managed home: the new-account defaults (skip, statusLine).
                let _ = provider::claude::bypass::default_new(&account.home);
                let _ = provider::claude::statusline_seed::default_new(
                    &account.home,
                    &sessionsation::alias::logical_words(&config.claude_default_statusline_command),
                );
            }
            return provider::dispatch::exec_login(
                &paths,
                &resolved.name,
                &config,
                resolved.provider,
                &account.home,
            );
        }
        Commands::Account {
            command:
                AccountCommand::Add {
                    platform,
                    config_dir,
                    no_login,
                    name,
                },
        } if provider::native::is_native(platform) => {
            anyhow::ensure!(
                config_dir.is_none() || ops::facts(platform).adopts_config_dir,
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
                None => ops::facts(platform)
                    .logged_in
                    .unwrap_or_default()
                    .to_owned(),
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
                return provider::dispatch::exec_login(
                    &paths,
                    &name,
                    &config,
                    platform,
                    &account.home,
                );
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
                print!("{}", account_table(&accounts));
            }
        }
        Commands::Account {
            command: AccountCommand::BypassWarning { name, skip, show },
        } => {
            let account = resolve_account(&paths, &name)?;
            daemon::ensure_started(&paths).await?;
            let value = daemon::call(
                &paths,
                "account/bypass-warning",
                json!({"name":account.name,"provider":account.provider,"skip":if skip {Some(true)} else if show {Some(false)} else {None}}),
            )
            .await?;
            print_json(&value)?;
        }
        Commands::Account {
            command: AccountCommand::Logout { name },
        } => {
            let account = resolve_account(&paths, &name)?;
            sessionsation::accounts::ensure_available(&paths, account.provider, &account.name)?;
            daemon::ensure_started(&paths).await?;
            let value = daemon::call(
                &paths,
                "account/logout",
                json!({"name":account.name,"provider":account.provider}),
            )
            .await?;
            println!(
                "{} logged out ({}).\nLog in again: sessionsation account login {}",
                account.id(),
                value["method"].as_str().unwrap_or("provider logout"),
                account.id()
            );
        }
        Commands::Account { command } => {
            daemon::ensure_started(&paths).await?;
            let (method, name, config_dir) = match command {
                // Native providers enroll through the arm above; the supervised one here.
                AccountCommand::Add {
                    platform,
                    config_dir,
                    name,
                    ..
                } if ops::facts(platform).supervised => (
                    "account/add",
                    AccountRef {
                        provider: platform,
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
                | AccountCommand::Logout { .. }
                | AccountCommand::Reorder { .. }
                | AccountCommand::LiveSteering { .. }
                | AccountCommand::BypassWarning { .. } => {
                    unreachable!("handled above")
                }
            };
            anyhow::ensure!(
                config_dir.is_none(),
                "INVALID_ARGUMENT: --config-dir is only supported with --platform claude"
            );
            sessionsation::accounts::ensure_available(&paths, name.provider, &name.name)?;
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
                // Config::parse drops a retired key and refuses an unknown one.
                config[&key] = serde_json::from_str(&value).unwrap_or(Value::String(value));
                let (validated, _) = Config::parse(config.clone())?;
                validated.validate()?;
                print_json(&daemon::call(&paths, "config/set", config).await?)?;
            } else {
                print_json(&config)?;
            }
        }
        Commands::Instances => {
            print_json(&serde_json::to_value(external::discover(&paths).await?)?)?
        }
        Commands::Archives => print_json(&external::opening::archives_snapshot(&paths)?)?,
        Commands::Open { session, name } => {
            print_json(&external::opening::open(&paths, &session, name.as_deref()).await?)?
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
                        ..Default::default()
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
                        ..Default::default()
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
                        ..Default::default()
                    },
                )
                .await?,
            )?)?;
        }
        Commands::Terminate {
            pane,
            pause,
            accept_risks,
            keep_pane,
        } => {
            daemon::ensure_started(&paths).await?;
            print_json(&serde_json::to_value(
                external::session_control::queue_termination(
                    &paths,
                    &pane,
                    external::MoveOptions {
                        pause,
                        accept_risks,
                        close_pane: !keep_pane,
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
                            ..Default::default()
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
        Commands::HandoffWorker { job } => external::worker(&paths, &job).await?,
        Commands::Run {
            host,
            herdr_session,
            account,
            thread,
            session,
            hosted,
            name,
            permission_mode,
            codex_options,
        } => {
            if let Some(account) = account {
                anyhow::ensure!(
                    session.is_none(),
                    "--session is reserved for managed launching"
                );
                let account = resolve_account(&paths, &account)?;
                anyhow::ensure!(
                    name.is_none()
                        || (!hosted
                            && thread.is_none()
                            && herdr_session.is_none()
                            && matches!(host, Host::Inline)),
                    "INVALID_ARGUMENT: --name names a new session started in this terminal (inline host, no --thread or --hosted)"
                );
                if hosted {
                    ops::require(
                        ops::facts(account.provider).hosted_sessions,
                        ops::HOSTED_CLAUDE_ONLY,
                    )?;
                    anyhow::ensure!(
                        matches!(host, Host::Inline)
                            && herdr_session.is_none()
                            && codex_options.is_empty(),
                        "INVALID_ARGUMENT: --hosted runs its viewer in this terminal (inline host, no provider options)"
                    );
                    return sessionsation::provider::claude::hosted::run_in_terminal(
                        &paths,
                        &account.name,
                        thread.as_deref(),
                        permission_mode,
                        None,
                    )
                    .await;
                }
                if matches!(host, Host::Inline)
                    && !hosted
                    && thread.is_none()
                    && herdr_session.is_none()
                {
                    ops::require(
                        codex_options.is_empty() || ops::facts(account.provider).cli_options,
                        ops::CLI_OPTIONS_CODEX_ONLY,
                    )?;
                    return sessionsation::inline::run_new(
                        &paths,
                        account.provider,
                        &account.name,
                        name.as_deref(),
                        &codex_options,
                    )
                    .await;
                }
                if provider::native::is_native(account.provider) && matches!(host, Host::Inline) {
                    anyhow::ensure!(
                        herdr_session.is_none(),
                        "--herdr-session requires --host herdr"
                    );
                    ops::require(codex_options.is_empty(), ops::CLI_OPTIONS_CODEX_ONLY)?;
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
                return Err(ops::unsupported(
                    "this account's sessions are not started here",
                ));
            }
        }
        Commands::Resume {
            session,
            expected_generation,
            confirm_unfinished,
            name,
        } => {
            sessionsation::api::resume_current(
                &paths,
                &session,
                &expected_generation,
                confirm_unfinished,
                name.as_deref(),
            )
            .await?
        }
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
        Commands::ProviderHook { .. } => unreachable!(),
    }
    Ok(())
}
/// Resolve NAME or PROVIDER:NAME against enrolled accounts (names are global).
/// An unknown bare name stays on the Codex path, whose launcher reports it.
fn resolve_account(paths: &Paths, id: &str) -> Result<AccountRef> {
    let (wanted, name) = AccountRef::parse_lookup(id)?;
    let state = if paths.root.join(sessionsation::layout::REGISTRY_DB).exists() {
        store::load_state_snapshot(paths).ok()
    } else {
        None
    };
    if let Some(state) = &state
        && !state.accounts_named(&name).is_empty()
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
/// `display (name)` when a display name differs from the stable name.
fn account_label(a: &Value) -> String {
    let name = a["name"].as_str().unwrap_or("?");
    match a["display_name"].as_str() {
        Some(display) if display != name => format!("{display} ({name})"),
        _ => name.to_owned(),
    }
}
/// Human account listing across providers. Unknown usage shows `-`, never 0%.
fn account_table(accounts: &[Value]) -> String {
    let percent = |a: &Value, window: &str| {
        a["usage"][window]["used_percent"]
            .as_f64()
            .map_or("-".to_owned(), |p| format!("{p:.0}%"))
    };
    let mut out = format!(
        "{:<24} {:<8} {:<12} {:>5} {:>5} {}\n",
        "NAME", "PROVIDER", "STATUS", "5H", "WEEK", "USAGE"
    );
    for a in accounts {
        let status = if a["membership"] == "removed" {
            "removed".to_owned()
        } else if a["enabled"] != true {
            "disabled".to_owned()
        } else if a["auth_required"] == true {
            a["auth_detail"]
                .as_str()
                .map_or("login".to_owned(), str::to_ascii_lowercase)
        } else {
            "ready".to_owned()
        };
        out.push_str(&format!(
            "{:<24} {:<8} {:<12} {:>5} {:>5} {}\n",
            account_label(a),
            a["provider"].as_str().unwrap_or("?"),
            status,
            percent(a, "short"),
            percent(a, "weekly"),
            a["usage_state"].as_str().unwrap_or("unknown"),
        ));
    }
    out
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn install_systemd(paths: &Paths) -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME missing")?;
    let executable = sessionsation::provider::claude::hooks::executable()?;
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

fn error_envelope(error: &anyhow::Error) -> serde_json::Value {
    let code = error_code(error);
    let message = if code == "DISK_FULL" {
        sessionsation::disk_space::disk_full_message(error)
    } else {
        sessionsation::messaging::recovery::diagnostic(&code)
            .map(|help| format!("{code}: {help}"))
            .unwrap_or_else(|| format!("{error:#}"))
    };
    let mut envelope = json!({"api_version":1,"ok":false,"error":{"code":code,"native":error.downcast_ref::<sessionsation::messaging::NativeReadFailure>(),"message":message,"retryable":matches!(code.as_str(),"ADAPTER_BUSY"|"OPERATION_BUSY"|"OBSERVATION_TIMEOUT"|"OPERATION_TIMEOUT"|"ATTACHMENT_UNOBSERVED"|"SESSION_OPERATION_IN_PROGRESS"|"STOP_TIMEOUT"|"AUTH_STALE")}});
    // Contract 1.63.0: a provider that cannot perform the operation at all.
    if let Some(u) = error.downcast_ref::<sessionsation::provider::stop_shape::Unsupported>() {
        envelope["error"]["supported"] = json!(false);
        envelope["error"]["reason"] = json!(u.0);
    }
    // Contract 1.60.0: a Claude token that could not be renewed right now.
    if code == "AUTH_STALE" {
        envelope["error"]["detail"] = json!(
            "a running Claude session on the account refreshes the token itself; the next attempt also renews it under Claude Code's refresh lock"
        );
    }
    // Contract 1.58.0: a session that has not written its first status-line record yet.
    if error
        .downcast_ref::<sessionsation::messaging::model_change::claude::HookNotRecorded>()
        .is_some()
    {
        envelope["error"]["retryable"] = json!(true);
        envelope["error"]["detail"] = json!("hook_not_recorded");
    }
    if let Some(space) = error.downcast_ref::<sessionsation::disk_space::InsufficientSpace>() {
        envelope["error"]["free_bytes"] = json!(space.free_bytes);
        envelope["error"]["needed_bytes"] = json!(space.needed_bytes);
        envelope["error"]["operation"] = json!(space.operation);
    }
    // Contract 1.49.0: a --wait that ended unconfirmed carries the operation record.
    if let Some(n) = error.downcast_ref::<sessionsation::session_model::NotConfirmed>() {
        envelope["error"]["operation"] = n.operation.clone();
        // Contract 1.55.0: a failed `--wait` operation is retryable when the account was busy.
        if n.code == "OPERATION_FAILED" && n.message.starts_with("ACCOUNT_BUSY") {
            envelope["error"]["retryable"] = json!(true);
        }
    }
    envelope
}

fn error_code(error: &anyhow::Error) -> String {
    if sessionsation::disk_space::is_disk_full(error) {
        return "DISK_FULL".into();
    }
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

#[cfg(test)]
mod tests;
