//! Public machine interface. The inherited CLI and native conversation IDs are
//! implementation details; consumers use stable catalog IDs and generations.
use crate::{
    catalog, daemon, external,
    paths::Paths,
    provider::{AccountRef, ProviderId, claude, native},
    store,
};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    time::Duration,
};

pub const VERSION: u32 = 1;
/// Contract semantic version: MAJOR breaking, MINOR additive (= `contract_revision`), PATCH fixes.
pub const CONTRACT_MAJOR: u32 = VERSION;
pub const CONTRACT_MINOR: u32 = 33;
pub const CONTRACT_PATCH: u32 = 0;
pub const CONTRACT_VERSION: &str = "1.33.0";

/// Accept `--api-version` MAJOR, MAJOR.MINOR or MAJOR.MINOR.PATCH when MAJOR matches.
pub fn parse_api_version(text: &str) -> Result<u32, String> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() > 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "invalid API version {text:?}; expected MAJOR[.MINOR[.PATCH]]"
        ));
    }
    let major: u32 = parts[0]
        .parse()
        .map_err(|_| format!("invalid API version {text:?}"))?;
    if major != CONTRACT_MAJOR {
        return Err(format!(
            "unsupported API major version {major}; this build serves contract major {CONTRACT_MAJOR} ({CONTRACT_VERSION})"
        ));
    }
    Ok(major)
}

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
        json!({"package":"sessionsation","package_version":env!("CARGO_PKG_VERSION"),"api_version":VERSION,"contract_revision":CONTRACT_MINOR,"contract_version":CONTRACT_VERSION,"executable":{"path":executable,"sha256":format!("{:x}",Sha256::digest(std::fs::read(&executable)?))}}),
    )
}

#[derive(Subcommand)]
pub enum Command {
    Capabilities,
    /// Batch host-scoped placement; no per-session host lookup required.
    Placements,
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
        /// Serve the last reconciled snapshot without transcript scans or process probes.
        #[arg(long)]
        cached: bool,
        #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u64).range(1..=3600))]
        interval: u64,
    },
    /// Resolve public session details against its current catalog generation,
    /// or run a session action (`session add-to-herdr ...`).
    #[command(args_conflicts_with_subcommands = true)]
    Session {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        expected_generation: Option<String>,
        #[command(subcommand)]
        action: Option<SessionAction>,
    },
    /// Resolve a current runtime attachment without provider or session discovery.
    Attachment {
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
    /// Read public assistant snapshots for an exact turn from a versioned stdin request.
    Checkpoints,
    /// Read the latest native turn identity/status; no sending permission required.
    TurnTarget {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    /// Resolve the active turn for explicit in-turn steering; never sends input.
    SteerTarget {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
    },
    /// Read a target/host request from stdin and validate an exited-process recovery plan.
    RecoveryPlan,
    /// Reconcile an interrupted recovery without launching or delivering input.
    RecoveryReconcile {
        message: String,
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
    /// Preview or request a generation-checked directory change or project-folder move.
    Relocate {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        to: std::path::PathBuf,
        #[arg(long)]
        move_files: bool,
        #[arg(long)]
        dry_run: bool,
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
        /// Open in a new tab of this existing Space instead of a new Space.
        #[arg(long)]
        #[arg(long, conflicts_with = "new_space")]
        space: Option<String>,
        /// Create a new Space with this label (exclusive with `--space`).
        #[arg(long, conflicts_with = "space")]
        new_space: Option<String>,
        /// Tab and catalog name (default: the current session name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Read existing Spaces from this session's own HERDR host, or from the named
    /// HERDR server (`--herdr-session`) for a session that is not in HERDR yet.
    Spaces {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        herdr_session: Option<String>,
        #[arg(long, requires = "herdr_session")]
        herdr_socket: Option<std::path::PathBuf>,
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
    /// Operator-only: abandon an uncertain or stuck resume-launch (terminal; sends nothing).
    AbandonOperation {
        #[arg(long)]
        operation: String,
        #[arg(long)]
        confirm: String,
    },
    /// Sessionsation-hosted Claude sessions (stream-json worker, viewer, permission prompts).
    Hosted {
        #[command(subcommand)]
        command: crate::provider::claude::hosted::Command,
    },
    /// Answer an agent's pending question (`pending_questions` in state); answers
    /// JSON from --answers or stdin. Idempotent; an attempted answer is never resent.
    Answer {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        request_id: String,
        /// `{"<question id>":{"option_ids":["o1"],"text":"..."}}`; read from stdin when absent.
        #[arg(long)]
        answers: Option<String>,
    },
    /// Read a versioned Submit/Queue/Resume/explicit Steer request from stdin.
    Send,
    Receipt {
        message: String,
    },
    Cancel {
        message: String,
    },
}

#[derive(Subcommand)]
pub enum SessionAction {
    /// Open a running session that has no HERDR terminal in a new tab of an
    /// existing Space (Claude background sessions via `claude attach`).
    AddToHerdr {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        /// HERDR server name.
        #[arg(long)]
        herdr_session: String,
        #[arg(long)]
        herdr_socket: Option<std::path::PathBuf>,
        /// Existing Space (workspace) id from `api spaces`.
        #[arg(
            long,
            conflicts_with = "new_space",
            required_unless_present = "new_space"
        )]
        space: Option<String>,
        /// Label of a Space to create (exclusive with `--space`).
        #[arg(long, conflicts_with = "space", required_unless_present = "space")]
        new_space: Option<String>,
        /// Tab name; defaults to the current session name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Restore stable session identity (merged ids, continuation lineage) that
    /// the identity record holds but the catalog lost, then reconcile. Idempotent;
    /// never deletes, recreates or renames an entry.
    RepairIdentity,
}

#[derive(Subcommand)]
pub enum Launcher {
    New {
        #[arg(long)]
        account: String,
        /// Claude: a Sessionsation-hosted (stream-json) session; the pane shows its viewer.
        #[arg(long)]
        hosted: bool,
        /// With --hosted: account (default) | default | acceptEdits | plan | bypassPermissions.
        #[arg(long, requires = "hosted")]
        permission_mode: Option<String>,
    },
    Resume {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        /// Claude: host the stopped conversation (stream-json) with a viewer instead of the TUI.
        #[arg(long)]
        hosted: bool,
        #[arg(long, requires = "hosted")]
        permission_mode: Option<String>,
    },
    Shell,
}

/// Resolve a native-provider (Claude or Gemini) catalog session and observe it
/// against its account home.
fn native_selected(
    paths: &Paths,
    id: &str,
    generation: &str,
) -> Result<(catalog::Session, claude::Observation)> {
    let s = catalog::resolve(paths, id, generation)?;
    let provider = native::of(&s.provider).context("UNSUPPORTED_PROVIDER_OPERATION")?;
    let account = native::account(paths, provider, &s.account)?;
    let observation = native::observe(
        provider,
        &account.home,
        &s.conversation_id,
        s.attachment.as_ref(),
    );
    Ok((s, observation))
}

/// The human `resume` gate: only the unfinished-history warning can be confirmed.
fn ensure_resumable_in_terminal(
    block_reason: Option<&str>,
    confirm_unfinished: bool,
) -> Result<()> {
    match block_reason {
        None => Ok(()),
        Some("unfinished") => {
            ensure!(
                confirm_unfinished,
                "UNFINISHED_HISTORY_CONFIRMATION_REQUIRED"
            );
            Ok(())
        }
        Some(_) => anyhow::bail!("SESSION_STATE_UNVERIFIED"),
    }
}

pub async fn resume_current(
    paths: &Paths,
    id: &str,
    generation: &str,
    confirm_unfinished: bool,
    name: Option<&str>,
) -> Result<()> {
    if let Some(name) = name {
        crate::launch::validate_session_name(name)?;
    }
    let native = catalog::resolve(paths, id, generation)
        .ok()
        .and_then(|s| native::of(&s.provider));
    // Everything that can be refused is refused before the journal is written.
    let (s, cwd) = if native.is_some() {
        let (s, observation) = native_selected(paths, id, generation)?;
        native::ensure_resumable(&s, &observation)?;
        crate::messaging::headless::ensure_idle(paths, &s.conversation_id)?;
        (s, std::env::current_dir()?)
    } else {
        let (s, source) = selected(paths, id, generation).await?;
        ensure!(
            source.inactive && s.attachment.is_none(),
            "SESSION_ALREADY_RUNNING"
        );
        ensure_resumable_in_terminal(source.block_reason.as_deref(), confirm_unfinished)?;
        ensure!(
            !matches!(s.account.as_str(), "external" | "unknown"),
            "ACCOUNT_UNAVAILABLE"
        );
        (s, std::env::current_dir()?)
    };
    if let Some(name) = name
        && name != s.name
    {
        catalog::check_name(paths, name, id)?;
    }
    let request =
        crate::inline::begin_resume(paths, &s, name.unwrap_or(&s.name), confirm_unfinished, &cwd)?;
    let result = async {
        let s = named(paths, s, generation, name)?;
        crate::inline::renamed(paths, &request, &s);
        if name.is_some() {
            crate::inline::label_tab(&s.name);
        }
        match native {
            Some(_) => native::exec_resume_session(paths, id, &s.generation()).await,
            None => {
                external::run_account_resume_requested(
                    paths,
                    &s.account,
                    &s.conversation_id,
                    (id, &s.generation()),
                    &request,
                    None,
                )
                .await
            }
        }
    }
    .await;
    if let Err(error) = &result {
        crate::inline::fail_resume(paths, &request, error);
    }
    result
}

fn hosted_flags(argv: &mut Vec<String>, hosted: bool, permission_mode: Option<String>) {
    if hosted {
        argv.push("--hosted".into());
        if let Some(mode) = permission_mode {
            argv.extend(["--permission-mode".into(), mode]);
        }
    }
}

async fn launcher(paths: &Paths, command: Launcher) -> Result<Value> {
    let mut argv = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "--data-dir".into(),
        paths.root.to_string_lossy().into_owned(),
    ];
    let mut cwd = None;
    match command {
        Launcher::New {
            account,
            hosted,
            permission_mode,
        } => {
            let account = account_ref(paths, &account)?;
            ensure!(
                !hosted || account.provider == ProviderId::Claude,
                "UNSUPPORTED_PROVIDER_OPERATION: hosted sessions are Claude-only"
            );
            let name = account.name.as_str();
            crate::accounts::ensure_available(paths, account.provider, name)?;
            let status = daemon::call(paths, "status", json!({})).await?;
            ensure!(
                status["accounts"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["name"] == name
                        && a["provider"].as_str().unwrap_or("codex") == account.provider.as_str()
                        && a["enabled"] == true
                        && a["auth_required"] == false)),
                "ACCOUNT_UNAVAILABLE"
            );
            // Codex keeps its historical bare-name argv; others are qualified.
            let target = match account.provider {
                ProviderId::Codex => name.to_owned(),
                _ => account.id(),
            };
            argv.extend(["run".into(), "--account".into(), target]);
            hosted_flags(&mut argv, hosted, permission_mode);
        }
        Launcher::Resume {
            session,
            expected_generation,
            hosted,
            permission_mode,
        } if catalog::resolve(paths, &session, &expected_generation)
            .is_ok_and(|s| native::of(&s.provider).is_some()) =>
        {
            let (s, observation) = native_selected(paths, &session, &expected_generation)?;
            native::ensure_resumable(&s, &observation)?;
            crate::messaging::headless::ensure_idle(paths, &s.conversation_id)?;
            cwd = Some(native::resume_directory(&s, &observation)?);
            if hosted {
                ensure!(
                    s.provider == ProviderId::Claude.as_str(),
                    "UNSUPPORTED_PROVIDER_OPERATION: hosted sessions are Claude-only"
                );
                argv.extend([
                    "run".into(),
                    "--account".into(),
                    format!("claude:{}", s.account),
                    "--thread".into(),
                    s.conversation_id.clone(),
                ]);
                hosted_flags(&mut argv, true, permission_mode);
            } else {
                argv.extend([
                    "resume".into(),
                    session,
                    "--expected-generation".into(),
                    expected_generation,
                ]);
            }
        }
        Launcher::Resume {
            session,
            expected_generation,
            hosted,
            ..
        } => {
            ensure!(
                !hosted,
                "UNSUPPORTED_PROVIDER_OPERATION: hosted sessions are Claude-only"
            );
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

/// The recorded Codex session directory, which must still be an absolute directory.
fn resume_cwd(cwd: &str) -> Result<std::path::PathBuf> {
    let p = std::path::PathBuf::from(cwd);
    ensure!(
        p.is_absolute() && p.is_dir(),
        "SAVED_DIRECTORY_UNAVAILABLE: {cwd}"
    );
    Ok(p)
}

#[derive(Subcommand)]
pub enum AccountCommand {
    List {
        /// Only accounts of this provider.
        #[arg(long, value_enum)]
        platform: Option<ProviderId>,
    },
    /// Enroll `codex:<name>` or `claude:<name>`; names are unique across providers.
    Add {
        id: String,
        /// Claude only: adopt this existing CLAUDE_CONFIG_DIR in place (never copied).
        #[arg(long)]
        config_dir: Option<std::path::PathBuf>,
    },
    Login {
        id: String,
    },
    /// Sign out with the provider's own mechanism; refused while the account is in use.
    Logout {
        id: String,
    },
    /// Gemini: show or set the authentication mode. `--mode google` selects Login with
    /// Google (Workspace) and returns the login plan; `--mode api-key --key-stdin`
    /// reads `{"api_key":"..."}` from stdin, stores it privately and validates it.
    Auth {
        id: String,
        #[arg(long, value_name = "google|api-key")]
        mode: Option<String>,
        #[arg(long, requires = "mode")]
        key_stdin: bool,
    },
    /// Move an account to a 0-based position in the shared account order.
    Reorder {
        id: String,
        #[arg(long)]
        position: u32,
    },
    /// Claude: probe `claude auth status` now and update the account auth state.
    Refresh {
        id: String,
    },
    /// "Rename only": change just the display name (id, folder and home never change;
    /// allowed while sessions run).
    Rename {
        id: String,
        #[arg(long, conflicts_with = "clear")]
        display_name: Option<String>,
        #[arg(long)]
        clear: bool,
        /// The row's `generation`; refuses with STALE_TARGET when the account changed.
        #[arg(long)]
        expected_generation: Option<String>,
    },
    /// "Rename & update backend": rename the display name, the account folder and
    /// the account id (`codex:old` -> `codex:NEW`). Without NEW_NAME the backend is
    /// renamed to match the existing display name ("Update backend"). Refused while
    /// any session of the account runs; journaled, never replayed.
    RenameBackend {
        id: String,
        new_name: Option<String>,
        /// The row's `generation` (required).
        #[arg(long)]
        expected_generation: String,
        /// Idempotency/operation id (a UUID); generated when omitted.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Claude: switch an adopted account to its managed home (no files are moved).
    ManagedHome {
        id: String,
    },
    /// Claude: show, enable or disable live steering (peer messages) for this account.
    /// Gemini: show, set or remove experimental.modelSteering (restart sessions to apply).
    LiveSteering {
        id: String,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
    /// Claude: show, skip or show-again the bypass-permissions (YOLO) startup warning.
    BypassWarning {
        id: String,
        #[arg(long, conflicts_with = "show")]
        skip: bool,
        #[arg(long)]
        show: bool,
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
/// Parse a qualified id and, for an operation on an existing account, name the
/// provider that does hold the name when this one does not. Names are unique per
/// provider, so enrolling (`account add`) skips this check.
fn account_ref(paths: &Paths, id: &str) -> Result<AccountRef> {
    let account = AccountRef::parse(id)?;
    if paths.root.join(crate::layout::REGISTRY_DB).exists()
        && let Ok(state) = store::load_state_snapshot(paths)
        && state.account(account.provider, &account.name).is_none()
        && let Some(other) = state.accounts_named(&account.name).first()
    {
        bail!(
            "ACCOUNT_PROVIDER_MISMATCH: {} is enrolled as {}:{}, not {}",
            account.name,
            other.provider,
            account.name,
            account.id()
        );
    }
    Ok(account)
}
/// Whether an account row can be chosen for new work.
fn account_selectable(a: &Value, removed: bool) -> bool {
    !removed && a["deletion_id"].is_null() && a["enabled"] == true && a["auth_required"] == false
}
/// Apply the last observed Gemini authentication failure to an account row: a
/// recorded failure marks it `auth_failed` and in need of a login; once cleared (a
/// successful run or login, or newer credentials) a failure-only marking is lifted.
fn project_gemini_auth_failure(paths: &Paths, name: &str, a: &mut Value) {
    use crate::provider::gemini::{auth_failure, auth_mode};
    let home = a["config_home"]
        .as_str()
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| crate::provider::native::default_home(paths, ProviderId::Gemini, name).ok());
    // Mode and key presence only (stat); the key itself is never read here.
    a["auth_mode"] = json!(home.as_deref().map(|h| auth_mode::mode(h).as_str()));
    a["api_key_present"] = json!(home.as_deref().map(auth_mode::key_present));
    let failure = home.and_then(|home| auth_failure::current(paths, name, &home));
    let fields = auth_failure::fields(failure.as_ref());
    for key in [
        "auth_failed",
        "auth_failure_reason",
        "auth_failure_observed_at",
    ] {
        a[key] = fields[key].clone();
    }
    if failure.is_some() {
        // Credentials exist but the provider rejected them: a login is needed.
        a["auth_required"] = json!(true);
        if a["auth_detail"].is_null() {
            a["auth_detail"] = json!("AUTH_FAILED");
        }
    } else if a["auth_detail"] == "AUTH_FAILED" {
        // The row was marked only for the failure, which no longer holds.
        a["auth_required"] = json!(false);
        a["auth_detail"] = Value::Null;
    }
}
/// Refresh the cheap, local Gemini auth-failure projection of a cached state's
/// accounts, so a failure recorded (or cleared) after the snapshot shows at once.
fn refresh_cached_accounts(paths: &Paths, value: &mut Value) {
    let Some(accounts) = value["accounts"].as_array_mut() else {
        return;
    };
    let stale_seconds = crate::store::load_config(paths)
        .map(|c| c.stale_seconds)
        .ok();
    for a in accounts {
        if a["provider"] != "gemini" {
            continue;
        }
        let Some(name) = a["name"].as_str().map(str::to_owned) else {
            continue;
        };
        if crate::paths::validate_name(&name).is_err() {
            continue;
        }
        project_gemini_auth_failure(paths, &name, a);
        let removed = a["membership"] == "removed";
        a["selectable"] = json!(account_selectable(a, removed));
        if let Some(stale) = stale_seconds {
            a["prefill"] = account_prefill(a, daemon::now(), stale);
        }
    }
}
pub fn project_accounts(paths: &Paths, accounts: Vec<Value>) -> Result<Vec<Value>> {
    // One process-table scan serves every Codex row.
    let codex_instances = accounts
        .iter()
        .any(|a| a["provider"].as_str().unwrap_or("codex") == "codex")
        .then(|| external::host_instances(paths).ok())
        .flatten();
    accounts
        .into_iter()
        .map(|mut a| {
            let name = &a["name"]
                .as_str()
                .context("INVALID_SUPERVISOR_RESPONSE")?
                .to_owned();
            let name = name.as_str();
            // A pre-provider supervisor could only report Codex accounts.
            let provider = ProviderId::parse(a["provider"].as_str().unwrap_or("codex"))?;
            let removed = crate::accounts::removed(paths, provider, name)?;
            a["id"] = json!(format!("{provider}:{name}"));
            a["provider"] = json!(provider);
            a["provider_capabilities"] = json!(crate::provider::capabilities(provider));
            a["membership"] = json!(if removed { "removed" } else { "enrolled" });
            // The account home as used now (contract 1.29.0): an adopted Claude
            // home, else the managed `accounts/<platform>/<name>`, or its legacy
            // path while the layout migration deferred that home (in use).
            let target = crate::layout::target_home(&paths.root, provider, name);
            let home = a["config_home"]
                .as_str()
                .filter(|h| !h.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| crate::layout::account_home(&paths.root, provider, name));
            a["home_deferred"] =
                json!(a["config_home"].as_str().is_none_or(str::is_empty) && home != target);
            a["home"] = json!(home);
            project_rename_state(
                paths,
                provider,
                name,
                &home,
                codex_instances.as_deref(),
                &mut a,
            );
            if let Some(id) = a["deletion_id"].as_str() {
                a["deletion"] = json!(crate::account_deletion::status(paths, id)?);
            }
            if provider == ProviderId::Gemini {
                project_gemini_auth_failure(paths, name, &mut a);
            } else {
                a["auth_mode"] = Value::Null;
                a["api_key_present"] = Value::Null;
            }
            if provider == ProviderId::Claude {
                if let Some(home) = a["config_home"]
                    .as_str()
                    .filter(|h| !h.is_empty())
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        crate::provider::native::default_home(paths, ProviderId::Claude, name).ok()
                    })
                {
                    let row = crate::provider::claude::bypass::row(&home);
                    for key in [
                        "bypass_warning",
                        "bypass_warning_value",
                        "bypass_warning_setting",
                    ] {
                        a[key] = row[key].clone();
                    }
                }
            }
            a["selectable"] = json!(account_selectable(&a, removed));
            a["prefill"] = account_prefill(
                &a,
                daemon::now(),
                crate::store::load_config(paths)?.stale_seconds,
            );
            Ok(a)
        })
        .collect()
}

/// Contract 1.30.0 account-name fields: `folder_name` (the backend name that
/// names the folder and the id), `name_mismatch`, `running_sessions` and
/// `backend_rename` (whether "Rename & update backend" can run now, and why not).
fn project_rename_state(
    paths: &Paths,
    provider: ProviderId,
    name: &str,
    home: &std::path::Path,
    codex_instances: Option<&[external::Instance]>,
    a: &mut Value,
) {
    let mismatch = a["display_name"].as_str().is_some_and(|d| d != name);
    a["folder_name"] = json!(name);
    a["name_mismatch"] = json!(mismatch);
    if a["generation"].is_null() {
        a["generation"] = json!(crate::account_rename::generation(
            provider.as_str(),
            name,
            a["display_name"].as_str(),
            a["config_home"].as_str().filter(|h| !h.is_empty()),
        ));
    }
    let running = if provider == ProviderId::Codex {
        codex_instances.map(|all| all.iter().filter(|i| i.home == home).count())
    } else if home.exists() {
        native::live_sessions(provider, home).ok().map(|l| l.len())
    } else {
        Some(0)
    };
    a["running_sessions"] = json!(running);
    let reason = if !a["deletion_id"].is_null() {
        Some("ACCOUNT_DELETION_IN_PROGRESS")
    } else if a["membership"] == "removed" {
        Some("ACCOUNT_REMOVED")
    } else if a["config_home"].as_str().is_some_and(|h| !h.is_empty()) {
        Some("ACCOUNT_HOME_ADOPTED")
    } else if crate::account_rename::unresolved_for(paths, provider, name)
        .ok()
        .flatten()
        .is_some()
    {
        Some("ACCOUNT_RENAME_UNRESOLVED")
    } else if running != Some(0) {
        Some("ACCOUNT_SESSIONS_RUNNING")
    } else {
        None
    };
    a["backend_rename"] = json!({"available":reason.is_none(),"reason":reason,
        "update_backend":mismatch,"update_backend_target":a["display_name"]});
}
/// Show or set a Gemini account's authentication mode (`api account auth` and
/// `account auth`). Setting `api_key` with a key stores it (0600, atomic) and runs
/// one headless validation; `google` returns the login plan. The key is never
/// part of the result or of any error.
pub async fn gemini_auth(
    paths: &Paths,
    account: &AccountRef,
    mode: Option<&str>,
    key: Option<crate::provider::gemini::auth_mode::ApiKey>,
) -> Result<Value> {
    use crate::provider::gemini::{self, auth_failure, auth_mode};
    ensure!(
        account.provider == ProviderId::Gemini,
        "UNSUPPORTED_PROVIDER_OPERATION: authentication modes are a Gemini account setting"
    );
    let mode = mode.map(auth_mode::Mode::parse).transpose()?;
    ensure!(
        key.is_none() || mode == Some(auth_mode::Mode::ApiKey),
        "INVALID_ARGUMENT: --key-stdin is only valid with --mode api-key"
    );
    let enrolled = gemini::account(paths, &account.name)?;
    let home = enrolled.home.clone();
    let mut changed = false;
    let mut validation = Value::Null;
    if let Some(mode) = mode {
        crate::accounts::ensure_available(paths, account.provider, &account.name)?;
        ensure!(
            home.is_dir(),
            "ACCOUNT_HOME_UNAVAILABLE: the Gemini home does not exist"
        );
        if let Some(key) = &key {
            auth_mode::store_key(&home, key)?;
            changed = true;
        }
        ensure!(
            mode != auth_mode::Mode::ApiKey || auth_mode::key_present(&home),
            "API_KEY_REQUIRED: no API key is stored for {}; pass one with --key-stdin",
            account.id()
        );
        changed |= auth_mode::set_mode(&home, mode)?;
        if changed {
            // A new key or mode starts with a clean slate; validation records afresh.
            auth_failure::clear(paths, &account.name);
        }
        if mode == auth_mode::Mode::ApiKey && changed {
            let config = crate::store::load_config(paths)?;
            validation = crate::messaging::headless::validate_gemini_key(
                paths,
                &config,
                &account.name,
                &home,
            )
            .await;
        }
    }
    // Let the supervisor observe the new state (auth_required) at once.
    daemon::ensure_started(paths).await?;
    let refreshed = daemon::call(
        paths,
        "account/refresh",
        json!({"name":account.name,"provider":account.provider}),
    )
    .await?;
    let current = auth_mode::mode(&home);
    let failure = auth_failure::current(paths, &account.name, &home);
    let running = gemini::live_sessions(&home).map(|l| l.len()).unwrap_or(0);
    let mut value = json!({"account_id":account.id(),"provider":account.provider,
        "auth_mode":current.as_str(),"api_key_present":auth_mode::key_present(&home),
        "auth_required":refreshed["auth_required"].as_bool().unwrap_or(true) || failure.is_some(),
        "changed":changed,"running_sessions":running,
        "applies_to":"sessions_started_after_the_change"});
    let fields = auth_failure::fields(failure.as_ref());
    for key in [
        "auth_failed",
        "auth_failure_reason",
        "auth_failure_observed_at",
    ] {
        value[key] = fields[key].clone();
    }
    if mode == Some(auth_mode::Mode::ApiKey) {
        value["validation"] = validation;
    }
    if current == auth_mode::Mode::Google {
        value["login_required"] = json!(!gemini::auth_status(&home).logged_in);
        if mode.is_some() {
            value["login"] = login_plan(paths, account)?["login"].clone();
        }
    }
    Ok(value)
}

/// The ordered Gemini authentication choices for clients (`auth_modes`). Personal
/// "Login with Google" stays listed but unavailable: Google ended Gemini CLI
/// login for personal accounts (all tiers) on 2026-06-18; the same OAuth flow
/// still serves Workspace accounts.
pub fn gemini_auth_modes() -> Value {
    json!([
        {"id":"google_personal","mode":"google","label":"Google login","available":false,
         "reason":"retired by Google for personal accounts — use Antigravity, API key or Workspace"},
        {"id":"google_workspace","mode":"google","label":"Google Workspace login","available":true,"reason":null},
        {"id":"api_key","mode":"api_key","label":"API key","available":true,"reason":null,
         "input":"stdin_json","field":"api_key"}
    ])
}

/// Exec plan for an interactive provider login; no URL or credential passes
/// through the client.
pub fn login_plan(paths: &Paths, account: &AccountRef) -> Result<Value> {
    Ok(
        json!({"account_id":account.id(),"provider":account.provider,"login":{
        "action":"exec","interactive":true,"credentials_in_client":false,
        "argv":[std::env::current_exe()?.to_string_lossy(),"--data-dir",paths.root.to_string_lossy(),"account","login",account.id()]}}),
    )
}

async fn account(paths: &Paths, command: AccountCommand) -> Result<Value> {
    match command {
        AccountCommand::Delete {
            id,
            confirm,
            request_id,
        } => {
            let account = account_ref(paths, &id)?;
            if let (Some(confirm), Some(request)) = (confirm, request_id) {
                crate::account_deletion::start(
                    paths,
                    account.provider,
                    &account.name,
                    &request,
                    &confirm,
                )
                .await
            } else {
                crate::account_deletion::preview(paths, account.provider, &account.name).await
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
            let account = account_ref(paths, &id)?;
            let name = account.name.as_str();
            crate::accounts::ensure_available(paths, account.provider, name)?;
            if let (Some(short), Some(weekly)) = (short_used, weekly_used) {
                ensure!(
                    !matches!(
                        account.provider,
                        ProviderId::Gemini | ProviderId::Antigravity
                    ),
                    "UNSUPPORTED_PROVIDER_OPERATION: Gemini and Antigravity report no usage, so there is nothing to place limits on"
                );
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
                    json!({"name":name,"provider":account.provider,"short":short,"weekly":weekly}),
                )
                .await?;
                Ok(json!({"account_id":id,"limits":value["thresholds"],"saved":value["saved"]}))
            } else {
                let status = daemon::call(paths, "status", json!({})).await?;
                let a = status["accounts"]
                    .as_array()
                    .and_then(|items| {
                        items.iter().find(|a| {
                            a["name"] == name
                                && a["provider"].as_str().unwrap_or("codex")
                                    == account.provider.as_str()
                        })
                    })
                    .context("ACCOUNT_NOT_FOUND")?;
                Ok(
                    json!({"account_id":id,"limits":a["switch_thresholds"],"saved":false,
                    "usage":a["usage"],"usage_fresh":a["usage_fresh"],"usage_state":a["usage_state"]}),
                )
            }
        }
        AccountCommand::List { platform } => {
            let v = daemon::call(paths, "status", json!({})).await?;
            Ok(json!(
                project_accounts(
                    paths,
                    v["accounts"]
                        .as_array()
                        .context("INVALID_SUPERVISOR_RESPONSE")?
                        .clone()
                )?
                .into_iter()
                .filter(|a| platform.is_none_or(|p| a["provider"] == p.as_str()))
                .collect::<Vec<_>>()
            ))
        }
        AccountCommand::Remove {
            id,
            confirm,
            revision,
        } => {
            let account = account_ref(paths, &id)?;
            if let (Some(confirm), Some(revision)) = (confirm, revision) {
                crate::accounts::remove(paths, account.provider, &account.name, &revision, &confirm)
                    .await
            } else {
                crate::accounts::preview_for(paths, account.provider, &account.name).await
            }
        }
        AccountCommand::Restore { id } => {
            let account = account_ref(paths, &id)?;
            crate::accounts::restore(paths, account.provider, &account.name)
        }
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
            let account = account_ref(paths, &id)?;
            daemon::call(
                paths,
                "account/history",
                json!({"name":account.name,"provider":account.provider,"start":start,"end":end,"step":step}),
            )
            .await
        }
        AccountCommand::Add { id, config_dir } => {
            let account = AccountRef::parse(&id)?;
            crate::paths::validate_new_account_name(&account.name)?;
            ensure!(
                config_dir.is_none() || account.provider == ProviderId::Claude,
                "INVALID_ARGUMENT: --config-dir is only supported for claude accounts"
            );
            crate::accounts::ensure_available(paths, account.provider, &account.name)?;
            daemon::ensure_started(paths).await?;
            let mut params = json!({"name":account.name,"provider":account.provider});
            if let Some(dir) = config_dir {
                params["config_dir"] = json!(dir);
            }
            let mut value = daemon::call(paths, "account/add", params).await?;
            if native::is_native(account.provider) {
                // The same plan `account login` returns, so a client can go
                // straight into the interactive login when it is required.
                value["login"] = login_plan(paths, &account)?["login"].clone();
                value["login_required"] = json!(!value["auth"]["problem"].is_null());
            }
            Ok(value)
        }
        AccountCommand::Rename {
            id,
            display_name,
            clear,
            expected_generation,
        } => {
            let account = account_ref(paths, &id)?;
            ensure!(
                clear || display_name.is_some(),
                "INVALID_ARGUMENT: pass --display-name NAME or --clear"
            );
            daemon::ensure_started(paths).await?;
            daemon::call(
                paths,
                "account/rename",
                json!({"name":account.name,"provider":account.provider,"display_name":if clear {None} else {display_name},"expected_generation":expected_generation}),
            )
            .await
        }
        AccountCommand::RenameBackend {
            id,
            new_name,
            expected_generation,
            request_id,
        } => {
            let account = account_ref(paths, &id)?;
            daemon::ensure_started(paths).await?;
            let request_id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            daemon::call(
                paths,
                "account/rename-backend",
                json!({"name":account.name,"provider":account.provider,"new_name":new_name,
                    "expected_generation":expected_generation,"request_id":request_id}),
            )
            .await
        }
        AccountCommand::LiveSteering {
            id,
            enable,
            disable,
        } => {
            let account = account_ref(paths, &id)?;
            daemon::ensure_started(paths).await?;
            daemon::call(
                paths,
                "account/live-steering",
                json!({"name":account.name,"provider":account.provider,"enable":if enable {Some(true)} else if disable {Some(false)} else {None}}),
            )
            .await
        }
        AccountCommand::BypassWarning { id, skip, show } => {
            let account = account_ref(paths, &id)?;
            daemon::ensure_started(paths).await?;
            daemon::call(
                paths,
                "account/bypass-warning",
                json!({"name":account.name,"provider":account.provider,"skip":if skip {Some(true)} else if show {Some(false)} else {None}}),
            )
            .await
        }
        AccountCommand::Reorder { id, position } => {
            let account = account_ref(paths, &id)?;
            crate::accounts::ensure_available(paths, account.provider, &account.name)?;
            daemon::ensure_started(paths).await?;
            daemon::call(
                paths,
                "account/reorder",
                json!({"name":account.name,"provider":account.provider,"position":position}),
            )
            .await
        }
        AccountCommand::Auth {
            id,
            mode,
            key_stdin,
        } => {
            let account = account_ref(paths, &id)?;
            let key = if key_stdin {
                let mut bytes = vec![];
                std::io::stdin().take(32769).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 32768,
                    "INVALID_REQUEST: the stdin request is too large"
                );
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct KeyRequest {
                    api_key: String,
                }
                // Never echo serde's message: it can quote the input.
                let request: KeyRequest = serde_json::from_slice(&bytes).map_err(|_| {
                    anyhow::anyhow!(
                        "INVALID_REQUEST: stdin must be the JSON object {{\"api_key\":\"...\"}}"
                    )
                })?;
                Some(crate::provider::gemini::auth_mode::ApiKey::new(
                    &request.api_key,
                )?)
            } else {
                None
            };
            gemini_auth(paths, &account, mode.as_deref(), key).await
        }
        AccountCommand::Login { id } if native::is_native(AccountRef::parse(&id)?.provider) => {
            let account = account_ref(paths, &id)?;
            native::account(paths, account.provider, &account.name)?;
            login_plan(paths, &account)
        }
        command => {
            let (method, id) = match command {
                AccountCommand::ManagedHome { id } => ("account/managed-home", id),
                AccountCommand::Refresh { id } => ("account/refresh", id),
                AccountCommand::Login { id } => ("account/login", id),
                AccountCommand::Logout { id } => ("account/logout", id),
                AccountCommand::Enable { id } => ("account/enable", id),
                AccountCommand::Disable { id } => ("account/disable", id),
                _ => unreachable!(),
            };
            let account = account_ref(paths, &id)?;
            crate::accounts::ensure_available(paths, account.provider, &account.name)?;
            daemon::ensure_started(paths).await?;
            daemon::call(
                paths,
                method,
                json!({"name":account.name,"provider":account.provider}),
            )
            .await
        }
    }
}

/// The Gemini provider entry of `api capabilities` (built apart: the whole document
/// exceeds the `json!` macro recursion limit).
fn gemini_provider() -> Value {
    json!({"id":"gemini","implemented":true,"enrollment":true,"adopt_config_dir":false,"login":"interactive_exec","usage":false,"usage_state":"unsupported","usage_source":crate::provider::gemini::USAGE_SOURCE,
        "new":true,"resume":true,"rename":true,"native_rename":false,"stop":false,
        "account_switch":false,"restart":false,"submit":true,"queue":true,
        "account_deletion":true,"account_removal":true,"account_rename":true,"account_refresh":true,"account_limits":false,"new_account_name_max":crate::paths::NEW_ACCOUNT_NAME_MAX,
        "headless_delivery":{"version":1,"command":"send","modes":["resume","new"],"semantics":"headless_print","requires_stopped_session":true,"timeout_seconds":{"default":crate::messaging::headless::DEFAULT_TIMEOUT_SECONDS,"max":crate::messaging::headless::MAX_TIMEOUT_SECONDS},"timeout_is_uncertain":true,"automatic_replay":false,"cancellation":false,"approval_mode":crate::messaging::headless::GEMINI_APPROVAL_MODE},
        "force_interrupt":false,"steer":true,"input_delivery":crate::messaging::claude_steer::gemini::input_delivery(),"cross_provider_conversion":false,
        "auth_modes":gemini_auth_modes(),"auth_command":{"since_revision":21,"command":"account auth","modes":["google","api_key"],"key_input":"stdin_json","validation_timeout_seconds":crate::messaging::headless::GEMINI_VALIDATION_SECONDS,"unsupported_modes":["vertex"]},
        "operations":crate::provider::capabilities(ProviderId::Gemini)})
}

/// The Antigravity provider entry of `api capabilities` (revision 24).
fn antigravity_provider() -> Value {
    json!({"id":"antigravity","implemented":true,"since_revision":24,"enrollment":true,"adopt_config_dir":false,"login":"interactive_exec","usage":false,"usage_state":"unsupported","usage_source":crate::provider::antigravity::USAGE_SOURCE,
        "new":true,"resume":true,"rename":true,"native_rename":false,"stop":true,
        "account_switch":false,"restart":false,"submit":true,"queue":true,
        "account_deletion":true,"account_removal":true,"account_rename":true,"account_refresh":true,"account_limits":false,"new_account_name_max":crate::paths::NEW_ACCOUNT_NAME_MAX,
        "headless_delivery":{"version":1,"command":"send","modes":["resume","new"],"semantics":"headless_print","requires_stopped_session":true,"timeout_seconds":{"default":crate::messaging::headless::DEFAULT_TIMEOUT_SECONDS,"max":crate::messaging::headless::MAX_TIMEOUT_SECONDS},"timeout_is_uncertain":true,"automatic_replay":false,"cancellation":false,"approval":"agy default (never --dangerously-skip-permissions)","print_timeout":true,"new_conversation":"empty conversation created first, without a prompt"},
        "force_interrupt":false,"steer":true,"input_delivery":crate::messaging::claude_steer::antigravity::input_delivery(),"cross_provider_conversion":false,
        "operations":crate::provider::capabilities(ProviderId::Antigravity)})
}

pub fn capabilities() -> Value {
    let mut value = json!({"api_version":VERSION,"package_version":env!("CARGO_PKG_VERSION"),"contract_revision":CONTRACT_MINOR,"version_discovery":true,"archive_filter":{"values":["active","archived","all"],"default":"active","unknown_retained":true},"providers":[
        {"id":"codex","implemented":true,"enrollment":true,"usage":true,
         "new":true,"resume":true,"account_switch":true,"restart":true,
         "submit":true,"queue":true,"delivery":"non_steering_native_queue",
         "account_deletion":true,"account_removal":true,"account_rename":true,"account_limits":true,"new_account_name_max":crate::paths::NEW_ACCOUNT_NAME_MAX,"purge":false,"force_interrupt":false,"steer":true,"cross_provider_conversion":false,"operations":crate::provider::capabilities(ProviderId::Codex)},
        {"id":"claude","implemented":true,"enrollment":true,"adopt_config_dir":true,"login":"interactive_exec","usage":true,"usage_source":claude::USAGE_SOURCE,
         "new":true,"resume":true,"rename":true,"native_rename":false,"stop":false,
         "account_switch":false,"restart":false,"submit":false,"queue":false,
         "account_deletion":true,"account_removal":true,"account_rename":true,"account_refresh":true,"account_limits":true,"new_account_name_max":crate::paths::NEW_ACCOUNT_NAME_MAX,"headless_delivery":{"version":1,"command":"send","modes":["resume","new"],"semantics":"headless_print","requires_stopped_session":true,"timeout_seconds":{"default":crate::messaging::headless::DEFAULT_TIMEOUT_SECONDS,"max":crate::messaging::headless::MAX_TIMEOUT_SECONDS},"timeout_is_uncertain":true,"automatic_replay":false,"cancellation":false},
         "force_interrupt":false,"steer":false,"cross_provider_conversion":false,
         "operations":crate::provider::capabilities(ProviderId::Claude)},
        gemini_provider(),
        antigravity_provider()
    ],"account_names":{"unique_across_providers":true,"id_format":"<provider>:<name>"},"attachment_lookup":{"version":1,"command":"attachment","discovery":false,"checks":["catalog_generation","pid_start_time"]},"relocation":{"version":1,"preview":true,"move_files":true,"non_steering":true},"launch":{"version":1,"validate":true,"start":true,"status":true,"list":true},"resume":{"version":1,"unfinished_confirmation":true,"directory_selection":true,"validate":true,"start":true,"status":true,"list":true},"hosts":["tmux","herdr"],"identity":"stable_backend_session_uuid",
    "mutations_require":"expected_generation","watch":"complete_ndjson_snapshots",
    "batch_observation":{"command":"observations","max_targets":64,"concurrency":8,"native_budget_seconds":5,"policies":["if_stale","refresh","cached_only"],"cache_hits_renew_evidence":false},
    "session_observation":{"command":"observe","native_timeout_seconds":5,"valid_for_seconds":5,"passive":true},
    "turn_observation":{"version":1,"command":"turn-target","includes_terminal_turns":true,"requires_sending_enabled":false,"sends_input":false,"starts_process":false,"association":"turn_only"},
    "steering":{"version":1,"automatic_compaction":"native_pending_until_continuation","dedicated_compact_turn":"native_rejects_without_injection","compaction_verified_profiles":["0.164.0","0.165.0","0.166.0"],"command":"send","mode":"steer","target_command":"steer-target","providers":["codex"],"requires_expected_turn_id":true,"requires_expected_runtime_generation":true,"requires_expected_generation":true,"acceptance_only":false,"reply_correlation":"after_steer_item","automatic_replay":false,"queue_fallback":false,"starts_process":false,"interrupts":false},"native_protocol":{"supported_versions":crate::messaging::NATIVE_PROFILES,"version_source":"initialize.userAgent","unknown_versions":"fail_closed"},"message_recovery":{"command":"send","mode":"resume","plan_command":"recovery-plan","reconcile_command":"recovery-reconcile","retained_live_attachment":true,"exited_process":true,"hosts":["herdr","tmux"],"explicit_only":true,"same_conversation":true,"launch_is_not_message_acceptance":true,"exact_item_idle_start":true,"ordinary_send_rejects_terminal_state":true,"start_ack_is_not_input_recorded":true,"owner_reservation_required":"message-recoveries"},
    "receipt_observation":{"poll_min_seconds":5,"public_correlation":true,"resumable_local_summary_scan":true,"native_budget_seconds":5,"max_turns":1000,"partial_match_is_not_delivery":true},
    "conversation_archive":{"version":1,"list":true,"archive":true,"restore":true,"delete":true,"compression":"gzip","storage":"sessionsation_data_root"},
    "placement":{"version":1,"command":"api placements","catalog_field":"placement","host_scoped":true,"last_known":true,"freshness_seconds":30},"space_move":{"version":1,"host":"herdr","existing_spaces":true,"preserves_process":true,"recovery":"read_only_reconciliation"},"checkpoints":{"version":1,"command":"api checkpoints","association":"turn_only","delivery_evidence":false,"page_size":50,"retained_events":256,"max_revisions":4096,"max_response_bytes":crate::messaging::checkpoints::MAX_RESPONSE_BYTES,"response_bytes_include":"json_envelope_and_newline","history_modes":["legacy","paginated"],"native_read_max_bytes":8388608,"legacy_turn_item_limit":8192},"session_names":{"version":1,"live_herdr_source":"host_tab_label","retained_when_unobserved":true,"host_labels_unique":false,"rename_updates_host":true},"session_details":true,"session_timing":true,"detach_stops_agent":false});
    for provider in value["providers"].as_array_mut().into_iter().flatten() {
        provider["account_logout"] = json!(provider["id"] != "antigravity");
    }
    value["contract_version"] = json!(CONTRACT_VERSION);
    value["state_cached"] = json!(true);
    // Revision 17: Claude and Gemini archive/restore/delete (moved native files).
    value["conversation_archive"]["providers"] =
        json!(["codex", "claude", "gemini", "antigravity"]);
    value["conversation_archive"]["native"] = json!({"version":1,"since_revision":17,"providers":["claude","gemini","antigravity"],
        "storage":"moved_native_files","compression":"none","restore_target":"original_account_home",
        "scope":"exact_conversation_ids_including_previous_conversation_ids","delete_removes_catalog_entry":true,
        "delete_removes_merged_aliases":true,"delete_staging":"deletions","dead_registry_entries_deleted":true,
        "credentials_touched":false,"session_capabilities":["archive","restore","delete"],
        "refusal_reasons":["SESSION_RUNNING","SESSION_HOSTED","SESSION_STATE_UNKNOWN","ACCOUNT_DELETED","HISTORY_UNAVAILABLE","HISTORY_IN_USE","HISTORY_SYMLINK","SESSION_ALREADY_ARCHIVED","SESSION_NOT_ARCHIVED","RESTORE_CONFLICT","ACCOUNT_HOME_CHANGED"]});
    // Revision 15: steer replies are attributed after the steer's own input item.
    value["steering"]["reply_observation"] = json!({"since_revision":15,"command":"receipt","native_method":"thread/turns/list","items_view":"full","scope":"steered_turn_only","anchor":"user_message_client_id","window":"until_next_user_input_or_turn_end","max_pages":8,"max_text_bytes":65536,"final_only_when_turn_completed":true});
    // Revision 18: bounded receipt stores with tombstoned retirement.
    let retention = crate::config::Config::default();
    // Revision 32: journaled inline run/resume with --name.
    value["inline_launch"] = crate::inline::capability();
    value["receipt_retention"] = json!({"since_revision":18,"capacity_per_store":10000,"default_terminal_retention_days":retention.receipt_retention_days,"default_unknown_horizon_days":retention.receipt_unknown_horizon_days,"age_from":"expires_at_or_last_update","horizon_state":"expired","horizon_reason":"outcome_unknown_at_retention_horizon","retired_ids":"tombstoned_never_reaccepted","retired_error":"RECEIPT_RETIRED","settings":["receipt_retention_days","receipt_unknown_horizon_days"],"recovery_journals_retained":true});
    // Revision 19: permission mode of newly started provider processes.
    value["launch_permissions"] = json!({"since_revision":19,"values":["yolo","default"],"built_in_default":crate::config::Config::default().launch_permissions.as_str(),"setting":"launch_permissions","request_field":"permissions","requests":["launch","resume-launch"],"cli_option":"--permissions","codex":{"yolo":{"approval_policy":"never","sandbox":"danger-full-access"}},"claude":{"yolo":"--dangerously-skip-permissions"},"gemini":{"yolo":"--approval-mode yolo"},"antigravity":{"yolo":"--dangerously-skip-permissions"},"applies_to":["new","resume","restart","account_switch","hosted"],"headless_delivery":"unchanged"});
    // Revision 20: folder trust pre-accepted for yolo Claude starts.
    value["claude_folder_trust"] = json!({"since_revision":20,"setting":"claude_pretrust_folders","built_in_default":crate::config::Config::default().claude_pretrust_folders,"applies_when":"launch_permissions_yolo","applies_to":["interactive","hosted"],"config_file":"CLAUDE_CONFIG_DIR/.claude.json","key":"projects.<directory>.hasTrustDialogAccepted","other_content":"preserved","symlink":"refused","credentials_read":false});
    // Revision 25: folder trust pre-accepted for yolo Antigravity starts.
    value["antigravity_folder_trust"] = json!({"since_revision":25,"setting":"antigravity_pretrust_folders","built_in_default":crate::config::Config::default().antigravity_pretrust_folders,"applies_when":"launch_permissions_yolo","applies_to":["interactive"],"config_file":"AGY_ACCOUNT_HOME/.gemini/antigravity-cli/settings.json","key":"trustedWorkspaces","match":"exact_directory","other_content":"preserved","symlink":"refused","credentials_read":false});
    // Revision 25: Antigravity stop, archive, restore and delete.
    value["antigravity_lifecycle"] = json!({"since_revision":25,"stop":{"method":"sigterm_exact_process","verified_by":["pid","process_start","command_line","account_home","conversation"],"wait_seconds":crate::provider::antigravity::STOP_SECONDS,"sigkill":false,"flushes_state":true,"errors":["SESSION_NOT_RUNNING","SESSION_STATE_UNVERIFIED","AMBIGUOUS_SESSION","STOP_TIMEOUT","STOP_UNAVAILABLE","STOP_FAILED"]},
        "archive":{"files":["conversations/<id>.db","conversations/<id>.db-wal","conversations/<id>.db-shm","brain/<id>/","annotations/<id>.pbtxt","presence/<id>.lock"],"index_row":"conversation_summaries.db row saved then removed in one SQLite transaction after a backup","delete_also_removes":["jetbox_summaries_proto.pb entry"],"unchanged":["history.jsonl","implicit/"],"match":"exact_conversation_id","refused_while":"a verified agy process (interactive or headless) holds the conversation"}});
    // Revision 21: per-account Claude bypass-permissions warning.
    value["claude_bypass_warning"] = json!({"since_revision":22,"setting":"skipDangerousModePermissionPrompt","config_file":"CLAUDE_CONFIG_DIR/settings.json","command":"account bypass-warning claude:NAME [--skip|--show]","values":["skip","show"],"new_accounts_default":"skip","account_row_fields":["bypass_warning","bypass_warning_value","bypass_warning_setting"],"event":"account.bypass_warning_skipped|shown","other_keys":"preserved","symlink":"refused","credentials_read":false});
    // 1.27.0: default statusLine for new managed Claude homes.
    value["data_layout"] = json!({"since_contract":"1.29.0","version":crate::layout::VERSION,"directories":[crate::layout::REGISTRY,crate::layout::SESSIONS,crate::layout::OPERATIONS,crate::layout::MESSAGING,crate::layout::CACHE,crate::layout::LOGS,crate::layout::ACCOUNTS],"account_home":"accounts/<platform>/<name>","account_fields":{"home":"the account home in use (adopted, managed, or a deferred legacy path)","home_deferred":"true while the layout migration left an in-use home at its legacy path"},"commands":["layout status","layout migrate --backup DIR [--dry-run]","layout rollback [--dry-run]"],"legacy_root":"LAYOUT_MIGRATION_REQUIRED","interrupted":"LAYOUT_MIGRATION_IN_PROGRESS"});
    value["retention"] = crate::retention::describe();
    // 1.30.0: per-provider account names, display/backend names and the backend rename.
    value["account_names"] = json!({"since_contract":"1.30.0","unique_across_providers":false,"unique_per_provider":true,"id_format":"<provider>:<name>","ambiguous_bare_name_error":"ACCOUNT_AMBIGUOUS",
        "row_fields":["folder_name","display_name","name_mismatch","running_sessions","generation","backend_rename"],
        "rename_only":{"command":"account rename","args":["id","--display-name|--clear","--expected-generation?"],"allowed_while_sessions_run":true,"changes":["display_name"]},
        "rename_backend":{"command":"account rename-backend","args":["id","new_name?","--expected-generation","--request-id?"],"update_backend_without_new_name":true,
            "refused_while_sessions_run":true,"journal":"operations/account-renames","phases":["admitted","moving_home","home_moved","registry_updated","complete","failed","uncertain","abandoned"],
            "replays":false,"recover":"recover","abandon":"abandon-operation","changes":["id","folder","registry","sessions","gemini_auth_failure","usage_history"],
            "unchanged":["receipts","digests","message_journals","operation_history"],"adopted_home":"ACCOUNT_HOME_ADOPTED",
            "errors":["ACCOUNT_SESSIONS_RUNNING","ACCOUNT_NAME_EXISTS","ACCOUNT_NAME_UNCHANGED","ACCOUNT_HOME_EXISTS","ACCOUNT_HOME_ADOPTED","ACCOUNT_REMOVED","ACCOUNT_RENAME_UNRESOLVED","ACCOUNT_DELETION_IN_PROGRESS","ACCOUNT_POLL_IN_PROGRESS","ACCOUNT_BUSY","STALE_TARGET","ACCOUNT_AMBIGUOUS","ACCOUNT_HOME_MOVE_FAILED","INVALID_ARGUMENT"]}});
    value["claude_default_statusline"] = json!({"since_contract":"1.27.0","setting":"claude_default_statusline_command","built_in_default":crate::config::Config::default().claude_default_statusline_command,"empty":"disabled","config_file":"CLAUDE_CONFIG_DIR/settings.json","key":"statusLine","value":{"type":"command","command":"<setting>"},"applies_to":"new_managed_homes_only","existing_key":"kept","other_keys":"preserved","symlink":"refused","credentials_read":false});
    // 1.28.0: operator abandon of uncertain/stuck resume-launch operations.
    value["abandon_operation"] = json!({"since_contract":"1.28.0","command":"abandon-operation","requires":["operation","confirm"],"applies_to":"resume_launch","phases":["uncertain","stuck"],"terminal_phase":"abandoned","replays":false,"sends":false,"refused_while_session_process_live":true});
    value["account_reorder"] = json!(true);
    value["session_identity"] = json!({"version":1,"native_id_reattachment":true,"switched_process_released":true,"claude_registry_field":"sessionId","gemini_process_source":"cmdline_session_id_argument","antigravity_process_source":"open_presence_lock_or_conversation_argument","codex_thread_source":"open_rollout_or_resume_argv","claude_continuation_marker":"continued-in","claude_fork_marker":"forkedFrom","continuation_link":"automatic_same_account","previous_conversation_ids":true,"merged_alias_error":"SESSION_MERGED","persisted_identity_record":true,"repair_command":"session repair-identity"});
    value["add_to_herdr"] = json!({"version":1,"command":"session add-to-herdr","requires":["session","expected_generation","herdr_session"],"requires_one_of":["space","new_space"],"optional":["name","herdr_socket"],"spaces_command":"spaces --herdr-session","providers":["claude"],"unsupported":{"gemini":"NO_NATIVE_ATTACH","antigravity":"NO_NATIVE_ATTACH","codex":"CODEX_TERMINAL_NOT_MOVABLE"},"claude_mechanism":"claude_attach_background_session","starts_agent":false,"verified_attachment":true,"automatic_replay":false});
    // Revision 20: create a new Space by label on resume, add-to-HERDR and move.
    value["new_space"] = json!({"since_revision":20,"version":1,"resume":true,"add_to_herdr":true,"move_space":true,"label_validation":"space_label","exists_error":"WORKSPACE_NAME_EXISTS"});
    value["resume_placement"] = json!({"version":1,"command":"resume","space":true,"name":true});
    if let Some(claude) = value["providers"]
        .as_array_mut()
        .and_then(|p| p.iter_mut().find(|p| p["id"] == "claude"))
    {
        for k in ["submit", "queue", "steer"] {
            claude[k] = json!(true);
        }
        claude["input_delivery"] = claude_input_delivery();
    }
    value["claude_input_delivery"] = claude_input_delivery();
    // Revision 17: Gemini submit/queue/steer as typed input into a HERDR pane.
    value["gemini_input_delivery"] = crate::messaging::claude_steer::gemini::input_delivery();
    // Revision 24: Antigravity submit/queue/steer as typed input into a HERDR pane.
    value["antigravity_input_delivery"] =
        crate::messaging::claude_steer::antigravity::input_delivery();
    value["questions"] = crate::questions::capability();
    value
}

/// Revision 16: Claude submit/queue/steer (hosted stream-json or live peer messages).
fn claude_input_delivery() -> Value {
    use crate::{messaging::claude_steer, provider::claude::hosted};
    json!({"version":1,"since_revision":16,"modes":["submit","queue","steer"],"priority":{"submit":"next","queue":"later","steer":"next"},
        "steer":crate::messaging::claude_steer::steer_semantics(),
        "session_capability":"capabilities.submit|queue|steer with capability_reasons and input_delivery.semantics",
        "hosted":{"semantics":hosted::SEMANTICS,"command":"hosted","launch":["launch start hosted","launcher new --hosted","launcher resume --hosted","run --hosted"],
            "permission_modes":hosted::PERMISSION_MODES,"default_permission_mode":"account","implicit_bypass":false,"permission_prompts":"pending_until_answered",
            "interrupt":"steering.interrupt","correlation":"command_lifecycle_and_result_user_message_uuids","viewer":"view","take_over":"take-over"},
        "peer_message":{"semantics":claude_steer::PEER_SEMANTICS,"label":claude_steer::PEER_LABEL,"opt_in":"account live-steering --enable","setting":"crossSessionInbound","interrupt":false,
            "correlation":"transcript_anchor_by_message_id","reply_requires_anchor":true},
        "terminal_input":{"semantics":claude_steer::terminal::SEMANTICS,"since_revision":17,"label":claude_steer::terminal::LABEL,
            "modes":["submit","steer"],"eligibility":"tracked running session whose attachment is a verified HERDR pane (attachment.host herdr, exact pid/start; a bg session through its exact claude attach client, the pane's only foreground process)","operator_switch":"config terminal_input_enabled (default true; off: TERMINAL_INPUT_DISABLED)",
            "preference":"before peer_message for submit and steer","recorded_as":"user record when idle; queued_command with origin human mid-turn",
            "steer":claude_steer::terminal_steer_semantics(),
            "keys":{"text":"pane send-text (no bracketed paste)","newline":"ctrl+j","clear":["ctrl+k","ctrl+u"],"submit":"enter",
                "leave_views":{"transcript":"ctrl+o","help":"?"},"never":["esc","ctrl+c","ctrl+d","ctrl+l"]},
            "escaped_prefixes":["/","!","#","?"],"max_text_bytes":claude_steer::terminal::MAX_TEXT,
            "draft":"read from the screen, cleared, typed back unsubmitted and verified; refused when not exactly reconstructable",
            "correlation":"human_transcript_record_with_typed_text","reply_requires_anchor":true,
            "refusal_reasons":claude_steer::terminal::REASONS,"automatic_replay":false},
        "delivery_request":{"field":"delivery","values":["auto","hosted_stream_json","terminal_input","peer_message"],"default":"auto"},
        "preference":["hosted_stream_json","terminal_input","peer_message"],
        "states":["pending_local","held","native_accepted","execution_started","completed_with_reply","completed_without_reply","failed","interrupted","cancelled","refused","rejected","expired","delivery_unknown"],
        "completion_requires":"result_and_completed_lifecycle",
        "timeout_seconds":{"default":claude_steer::DEFAULT_TIMEOUT_SECONDS,"max":3600},"automatic_replay":false,"cancellation":"before_delivery_only"})
}

fn public_session(s: catalog::Session) -> Value {
    let generation = s.generation();
    let account_id = format!("{}:{}", s.provider, s.account);
    let mut value = json!(s);
    value["account_id"] = json!(account_id);
    value["generation"] = json!(generation);
    value["name_source"] = json!(if value["attachment"]["host"] == "herdr" {
        "herdr_last_observed"
    } else {
        "catalog"
    });
    value
}

/// Native-provider (Claude or Gemini) session projection. Sessionsation metadata (`name`, `name_source`,
/// catalog `directory`) stays distinct from native facts (`native_*`, pid).
/// Availability is running/stopped/stale/unknown; never derived from a title.
fn native_public(
    provider: ProviderId,
    s: catalog::Session,
    observed: Option<&claude::Observation>,
    account_deleted: bool,
) -> Value {
    // A deleted account's conversations stay listed as preserved history.
    let availability = match observed {
        Some(o) => o.availability,
        None if account_deleted => "stopped",
        None => "unknown",
    };
    let running = availability == "running";
    let directory = observed
        .and_then(|o| o.directory.clone())
        .or_else(|| s.directory.clone());
    let attached = s.attachment.is_some();
    let conversation = s.conversation_id.clone();
    let (add_to_herdr, add_reason) = match observed {
        Some(o) if !account_deleted => native::add_to_herdr_capability(provider, &s, o),
        _ => (false, Some(claude::attach::NOT_RUNNING)),
    };
    // name_source follows the same attachment rule as Codex (public_session).
    let mut value = public_session(s);
    value["conversation_id_source"] = json!("native");
    value["availability"] = json!(availability);
    value["native_status"] = json!(observed.and_then(|o| o.native_status.clone()));
    value["native_name"] = json!(observed.and_then(|o| o.native_name.clone()));
    value["native"] = json!({"conversation_id":conversation,"status":value["native_status"],"name":value["native_name"],
        "kind":observed.and_then(|o| o.native_kind.clone()),"job_id":observed.and_then(|o| o.job_id.clone())});
    // Lineage evidence: `previous_conversation_ids` (continued-in markers, linked
    // into this entry) and a branch source that remains a separate conversation.
    value["previous_conversation_ids"] = json!(
        value["previous_conversation_ids"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    );
    value["forked_from_conversation_id"] = json!(observed.and_then(|o| o.forked_from.clone()));
    value["pid"] = json!(observed.and_then(|o| o.pid));
    value["process_start"] = json!(observed.and_then(|o| o.process_start));
    value["host_known"] = json!(attached);
    value["directory"] = json!(directory);
    value["conversation_created_at"] = json!(observed.and_then(|o| o.started_at.clone()));
    value["history_file_bytes"] = json!(observed.and_then(|o| o.transcript_bytes));
    value["discovery_warnings"] = json!(
        observed
            .and_then(|o| o.detail.clone())
            .into_iter()
            .collect::<Vec<_>>()
    );
    value["activity"] = Value::Null;
    value["last_accessed"] = json!(
        observed
            .and_then(|o| o.transcript.as_deref())
            .and_then(|t| crate::provider::activity::last_accessed(provider, t))
    );
    value["active_since"] = json!(
        observed
            .filter(|o| o.availability == "running")
            .and_then(|o| o.active_since)
    );
    value["archive"] = json!({"kind":"none","archived":false,"state":"active","archived_at":null,"source":"provider_unsupported"});
    value["archived"] = json!(false);
    value["resume_block_reason"] = json!(match availability {
        "stopped" => None,
        "running" => Some("already_running"),
        "stale" => Some("history_unavailable"),
        _ => Some("unverified_history"),
    });
    let caps = crate::provider::capabilities(provider);
    let usable_directory = directory
        .as_deref()
        .is_some_and(|d| std::path::Path::new(d).is_absolute() && std::path::Path::new(d).is_dir());
    let resumable = availability == "stopped" && usable_directory && !account_deleted;
    if account_deleted {
        value["resume_block_reason"] = json!("account_deleted");
        value["history_preserved"] = json!(true);
    }
    value["capabilities"] = json!({"rename":true,"native_rename":false,"resume":resumable,
        "focus":running && attached,"switch_account":false,"restart":false,"stop":provider == ProviderId::Antigravity && running && observed.is_some_and(|o| o.pid.is_some()),"move_space":false,
        "change_directory":false,"move_project":false,"archive":false,"delete":false,
        "submit":false,"queue":false,"steer":false,"add_to_herdr":add_to_herdr});
    let resume_reason = if account_deleted {
        Some("ACCOUNT_DELETED")
    } else {
        (availability == "stopped" && !usable_directory).then_some("SAVED_DIRECTORY_UNAVAILABLE")
    };
    value["capability_reasons"] = json!({"resume":resume_reason,"native_rename":caps.native_rename.reason,"switch_account":caps.account_switch.reason,
        "restart":caps.restart.reason,"stop":caps.stop.reason,"move_space":caps.move_space.reason,
        "change_directory":caps.relocate.reason,"move_project":caps.relocate.reason,"archive":caps.archive.reason,
        "delete":caps.delete_session.reason,"submit":caps.submit.reason,"queue":caps.queue.reason,"steer":caps.steer.reason,"add_to_herdr":add_reason});
    if provider == ProviderId::Antigravity {
        // Stop is available exactly while a verified agy process runs the conversation.
        value["capability_reasons"]["stop"] = if value["capabilities"]["stop"] == true {
            Value::Null
        } else {
            json!(if running {
                "SESSION_STATE_UNVERIFIED: no verified Antigravity process"
            } else {
                "SESSION_NOT_RUNNING"
            })
        };
    }
    value
}

async fn inventory(paths: &Paths) -> Result<Vec<external::Instance>> {
    let items = external::discover_catalog(paths).await?;
    catalog::reconcile(paths, &items)?;
    catalog::names::refresh(paths, &items).await?;
    Ok(items)
}
/// The verified discovery source of a Codex session (read-only; no catalog write).
pub(crate) async fn codex_source(
    paths: &Paths,
    s: &catalog::Session,
) -> Result<Option<external::Instance>> {
    let items = external::discover_catalog(paths).await?;
    Ok(items.into_iter().find(|i| source_matches(s, i)))
}
pub(crate) fn source_matches(s: &catalog::Session, i: &external::Instance) -> bool {
    i.thread_id == s.conversation_id
        && i.account == s.account
        && s.attachment.as_ref().is_none_or(|a| {
            !i.inactive && a.target == i.pane && a.pid == i.pid && a.process_start == i.start_time
        })
}

pub(crate) fn attachment_started_at(now: i64, uptime: f64, ticks: u64, hz: i64) -> Option<i64> {
    if hz <= 0 || ticks == 0 || !uptime.is_finite() {
        return None;
    }
    let age = uptime - ticks as f64 / hz as f64;
    (age >= 0. && age <= now as f64).then(|| now - age.floor() as i64)
}
pub(crate) fn boot_clock() -> Option<(f64, i64)> {
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
/// A snapshot older than this is reported `fresh:false`.
pub const SNAPSHOT_FRESH_SECONDS: i64 = 15;
fn snapshot_file(paths: &Paths, filter: ArchiveFilter) -> std::path::PathBuf {
    let name = match filter {
        ArchiveFilter::Active => "active",
        ArchiveFilter::Archived => "archived",
        ArchiveFilter::All => "all",
    };
    crate::layout::state_snapshot(&paths.root, name)
}
/// Every full `state` leaves its result for `state --cached` (atomic replace;
/// best effort, never the catalog).
fn write_snapshot(paths: &Paths, filter: ArchiveFilter, value: &Value) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        let _ = crate::paths::atomic_write(&snapshot_file(paths, filter), &bytes);
    }
}
/// `api state --cached`: the last full result with no catalog write, transcript
/// scan or process probe. Without a snapshot a full state is computed.
pub async fn state_cached(paths: &Paths, filter: ArchiveFilter) -> Result<Value> {
    let read = std::fs::read(snapshot_file(paths, filter))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|v| v["observed_at"].is_i64() && v["sessions"].is_array());
    let Some(mut value) = read else {
        return state_filtered(paths, filter).await;
    };
    let observed_at = value["observed_at"].as_i64().unwrap_or(0);
    value["snapshot"] = json!({"observed_at":observed_at,"fresh":daemon::now() - observed_at <= SNAPSHOT_FRESH_SECONDS,"cached":true});
    value["capabilities"] = capabilities();
    refresh_cached_accounts(paths, &mut value);
    Ok(value)
}
pub async fn state_filtered(paths: &Paths, filter: ArchiveFilter) -> Result<Value> {
    let started = std::time::Instant::now();
    // Inventory and the supervisor's account status are independent reads.
    let inventory_task = async {
        let items = inventory(paths).await;
        (items, started.elapsed().as_millis())
    };
    let status_task = async {
        let status_started = std::time::Instant::now();
        let status = match tokio::time::timeout(
            Duration::from_secs(1),
            daemon::call(paths, "status", json!({})),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("Account status snapshot timed out")),
        };
        (status, status_started.elapsed().as_millis())
    };
    let ((items, inventory_ms), (status, account_status_ms)) =
        tokio::join!(inventory_task, status_task);
    let items = items?;
    let (accounts, source) = match status {
        Ok(v) => (
            v["accounts"]
                .as_array()
                .context("INVALID_SUPERVISOR_RESPONSE")?
                .clone(),
            "supervisor",
        ),
        Err(_) if paths.root.join(crate::layout::REGISTRY_DB).exists() => {
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
    let observed = native::reconcile(paths).unwrap_or_default();
    let catalog_sessions = catalog::list(paths)?;
    let placements = catalog::placement::project(paths, &catalog_sessions)?;
    let archives = external::archiving::metadata(paths)?;
    let mut deleted_accounts = std::collections::HashMap::new();
    let mut sessions=catalog_sessions.into_iter().filter(|s|archives.get(&s.id).is_none_or(|a|a["state"]!="deleted")).map(|s| {
        if let Some(provider) = native::of(&s.provider) {
            let id = s.id.clone();
            let deleted = !observed.contains_key(&id)
                && *deleted_accounts
                    .entry((provider, s.account.clone()))
                    .or_insert_with(|| native::deleted(paths, provider, &s.account).unwrap_or(false));
            let mut value = native_public(provider, s, observed.get(&id).map(|(o, _)| o), deleted);
            if provider == ProviderId::Claude {
                crate::messaging::claude_steer::apply_session_input(paths, &mut value, observed.get(&id).map(|(_, h)| h.as_path()));
            } else if provider == ProviderId::Gemini {
                crate::messaging::claude_steer::gemini::apply_session_input(&mut value, observed.get(&id).map(|(_, h)| h.as_path()), crate::messaging::claude_steer::terminal::enabled(paths));
            } else if provider == ProviderId::Antigravity {
                crate::messaging::claude_steer::antigravity::apply_session_input(&mut value, observed.get(&id).map(|(_, h)| h.as_path()), crate::messaging::claude_steer::terminal::enabled(paths));
            }
            external::archiving::native::apply_session(paths, provider, &mut value, observed.get(&id).map(|(_, h)| h.as_path()), archives.get(&id));
            let running = value["availability"] == "running";
            value["pending_questions"] = match provider {
                ProviderId::Claude if running => {
                    let conversation = value["conversation_id"].as_str().unwrap_or_default().to_owned();
                    crate::questions::to_values(crate::questions::claude_pending(paths, &id, &conversation, observed.get(&id).map(|(_, h)| h.as_path())))
                }
                _ => json!([]),
            };
            if provider == ProviderId::Gemini {
                value["pending_questions_unsupported_reason"] = json!(crate::questions::NO_DETECTION_GEMINI);
            } else if provider == ProviderId::Antigravity {
                value["pending_questions_unsupported_reason"] = json!(crate::questions::NO_DETECTION_ANTIGRAVITY);
            }
            value["placement"] = placements["memberships"][id.as_str()].clone();
            return value;
        }
        let source=items.iter().find(|i|source_matches(&s,i));
        let known=!matches!(s.account.as_str(),"external"|"unknown");
        let resume_supported=s.provider=="codex";
        let attached=s.attachment.is_some();
        let archive = archive_info(&s, source, &archives);
        let archived=archive["archived"]==true;
        let mut value=public_session(s);
        value["placement"]=placements["memberships"][value["id"].as_str().unwrap_or("")].clone();
        value["archive"]=archive;
        value["archived"]=value["archive"]["archived"].clone();
        value["resume_block_reason"]=json!(if attached && source.is_none() {Some("host_unavailable")}else{resume_block_reason(resume_supported,source)});
        let running=source.is_some_and(|i|!i.inactive);
        let saved=source.is_some_and(|i|i.inactive && matches!(i.block_reason.as_deref(), None | Some("unfinished" | "unknown")) && i.move_id.is_none());
        value["availability"]=json!(if archived {"archived"}else if running {"running"}else if saved {"saved"}else{"unavailable"});
        let facts=source.map(|s|external::history_file_details(&s.rollout)).unwrap_or(Value::Null);
        value["directory"]=source.map(|s|json!(s.cwd)).unwrap_or(Value::Null);
        value["discovery_warnings"]=source.map(|s|json!(s.warnings)).unwrap_or(json!([]));
        value["conversation_created_at"]=facts["conversation_created_at"].clone();
        value["history_file_bytes"]=facts["history_file_bytes"].clone();
        value["activity"]=source.map(|s|json!(s.activity)).unwrap_or(Value::Null);
        value["pending_questions"]=match source.filter(|i|!i.inactive && !archived) {
            Some(i)=>crate::questions::to_values(crate::questions::codex_pending(paths,value["id"].as_str().unwrap_or_default(),&i.rollout)),
            None=>json!([]),
        };
        value["last_accessed"]=source.and_then(|s|s.last_accessed).map_or(Value::Null,|n|json!(n));
        value["active_since"]=json!(source.filter(|s|!s.inactive).and_then(|s|clock.and_then(|(uptime,hz)|attachment_started_at(now,uptime,s.start_time,hz))));
        let unambiguous=items.iter().filter(|i|!i.inactive && i.thread_id==value["conversation_id"]).count()<=1;
        value["capabilities"]=json!({"rename":true,"move_space":running && unambiguous && source.is_some_and(|s|s.pane.starts_with("herdr/")),"resume":resume_supported && saved,"change_directory":known && running && unambiguous,"move_project":known && running && unambiguous,"switch_account":known && running && unambiguous,"restart":known && running && unambiguous,"stop":known && running && unambiguous,"archive":source.is_some_and(|i|i.inactive),"delete":source.is_some_and(|i|i.inactive)});
        let (_, add_reason) = claude::attach::codex_capability(running, source.is_some_and(|s|s.pane.starts_with("herdr/")));
        value["capabilities"]["add_to_herdr"]=json!(false);
        value["capability_reasons"]=json!({"add_to_herdr":add_reason});
        value["previous_conversation_ids"]=json!(value["previous_conversation_ids"].as_array().cloned().unwrap_or_default());
        if archived { value["capabilities"]=json!({"rename":false,"add_to_herdr":false,"change_directory":false,"move_project":false,"resume":false,"switch_account":false,"move_space":false,"restart":false,"stop":false,"archive":false,"delete":value["archive"]["kind"]=="managed","restore":value["archive"]["kind"]=="managed"}); }
        let ctx = CodexReasonContext { running, unambiguous, known, in_herdr: source.is_some_and(|s|s.pane.starts_with("herdr/")), resume_supported, archived };
        fill_codex_reasons(&mut value, &ctx);
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
                "change_directory",
                "move_project",
                "restart",
                "stop",
                "archive",
                "delete",
            ] {
                session["capabilities"][capability] = json!(false);
                session["capability_reasons"][capability] = json!("SESSION_OPERATION_IN_PROGRESS");
            }
            session["resume_block_reason"] = json!("moving");
        }
    }
    let observed_at = daemon::now();
    let value = json!({"observed_at":observed_at,"account_source":source,
        "accounts":accounts,"sessions":sessions,"hosts":placements["hosts"],
        "snapshot":{"observed_at":observed_at,"fresh":true,"cached":false},
        "timing":{"inventory_ms":inventory_ms,"account_status_ms":account_status_ms,"total_ms":started.elapsed().as_millis()},
        "capabilities":capabilities()});
    write_snapshot(paths, filter, &value);
    Ok(value)
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
pub(crate) struct CodexReasonContext {
    pub running: bool,
    pub unambiguous: bool,
    pub known: bool,
    pub in_herdr: bool,
    pub resume_supported: bool,
    pub archived: bool,
}

/// Gives every false Codex capability a stable reason code (existing reasons are kept).
pub(crate) fn fill_codex_reasons(value: &mut Value, c: &CodexReasonContext) {
    let caps = value["capabilities"].clone();
    for key in [
        "move_space",
        "resume",
        "change_directory",
        "move_project",
        "switch_account",
        "restart",
        "stop",
        "archive",
        "delete",
        "restore",
        "rename",
    ] {
        if caps[key] != json!(false) || !value["capability_reasons"][key].is_null() {
            continue;
        }
        let reason = if c.archived {
            "SESSION_ARCHIVED"
        } else {
            match key {
                "resume" if !c.resume_supported => "UNSUPPORTED_PROVIDER_OPERATION",
                "resume" if c.running => "SESSION_ALREADY_RUNNING",
                "resume" => "HISTORY_UNAVAILABLE",
                "archive" | "delete" if c.running => "SESSION_RUNNING_STOP_FIRST",
                "archive" | "delete" => "HISTORY_UNAVAILABLE",
                "restore" => "NOT_ARCHIVED",
                "rename" => "RENAME_UNAVAILABLE",
                _ if !c.known => "ACCOUNT_UNKNOWN",
                _ if !c.running => "SESSION_NOT_RUNNING",
                _ if !c.unambiguous => "AMBIGUOUS_SESSION",
                "move_space" if !c.in_herdr => "NOT_IN_HERDR",
                _ => "UNAVAILABLE",
            }
        };
        value["capability_reasons"][key] = json!(reason);
    }
}

fn resume_block_reason(
    supported: bool,
    source: Option<&external::Instance>,
) -> Option<&'static str> {
    if !supported {
        return Some("unsupported_provider");
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

pub fn attachment(paths: &Paths, id: &str, generation: &str) -> Result<Value> {
    let session = catalog::resolve(paths, id, generation)?;
    let runtime = session.attachment.as_ref().context("SESSION_NOT_ATTACHED: This session has no live terminal. Refresh the session list; Focus never starts an agent.")?;
    ensure!(
        external::attachment_is_live(runtime.pid, runtime.process_start).unwrap_or(false),
        "SESSION_ATTACHMENT_UNAVAILABLE: The original process is no longer verified. Refresh the session list; Focus never starts an agent."
    );
    let mut value = public_session(session);
    value["observed_at"] = json!(daemon::now());
    value["availability"] = json!("running");
    value["scope"] = json!("catalog_generation_and_process_identity");
    value["starts_process"] = json!(false);
    value["sends_input"] = json!(false);
    Ok(value)
}

/// `api session repair-identity`: restore what the identity record holds, link
/// verified continuations, and report ids that other on-disk evidence names but
/// no catalog entry or alias resolves (never invented, never recreated).
pub fn repair_identity(paths: &Paths) -> Result<Value> {
    let before = catalog::aliases(paths)?;
    let healed = catalog::repair_identity(paths)?;
    native::reconcile(paths)?;
    let after = catalog::aliases(paths)?;
    let sessions = catalog::list(paths)?;
    let merged: Vec<Value> = after
        .iter()
        .filter(|(alias, _)| !before.contains_key(*alias))
        .map(|(alias, into)| json!({"id":alias,"merged_into":into}))
        .collect();
    let mut unresolved: Vec<String> = catalog::placement::evidence_ids(paths)?
        .into_iter()
        .filter(|id| !after.contains_key(id) && !sessions.iter().any(|s| s.id == *id))
        .collect();
    unresolved.sort();
    unresolved.dedup();
    Ok(json!({
        "restored_aliases": healed.restored_aliases,
        "restored_lineage": healed.restored_lineage,
        "merged": merged,
        "aliases": after.len(),
        "lineages": sessions.iter().filter(|s| !s.previous_conversation_ids.is_empty()).count(),
        "unresolved_ids": unresolved,
        "unresolved_reason": "no catalog entry, alias or identity record names these ids; they are not recreated",
    }))
}

pub async fn session_details(paths: &Paths, id: &str, generation: &str) -> Result<Value> {
    let items = inventory(paths).await?;
    let observed = native::reconcile(paths).unwrap_or_default();
    let session = catalog::resolve(paths, id, generation)?;
    if let Some(provider) = native::of(&session.provider) {
        let placement =
            catalog::placement::project(paths, std::slice::from_ref(&session))?["memberships"]
                [&session.id]
                .clone();
        let account_id = format!("{}:{}", session.provider, session.account);
        let deleted = !observed.contains_key(id)
            && native::deleted(paths, provider, &session.account).unwrap_or(false);
        let mut value = native_public(provider, session, observed.get(id).map(|(o, _)| o), deleted);
        if provider == ProviderId::Claude {
            crate::messaging::claude_steer::apply_session_input(
                paths,
                &mut value,
                observed.get(id).map(|(_, h)| h.as_path()),
            );
        } else if provider == ProviderId::Gemini {
            crate::messaging::claude_steer::gemini::apply_session_input(
                &mut value,
                observed.get(id).map(|(_, h)| h.as_path()),
                crate::messaging::claude_steer::terminal::enabled(paths),
            );
        } else if provider == ProviderId::Antigravity {
            crate::messaging::claude_steer::antigravity::apply_session_input(
                &mut value,
                observed.get(id).map(|(_, h)| h.as_path()),
                crate::messaging::claude_steer::terminal::enabled(paths),
            );
        }
        external::archiving::native::apply_session(
            paths,
            provider,
            &mut value,
            observed.get(id).map(|(_, h)| h.as_path()),
            external::archiving::metadata(paths)?.get(id),
        );
        value["placement"] = placement;
        value["account_id"] = json!(account_id);
        value["original_host"] = Value::Null;
        return Ok(value);
    }
    let source = items.iter().find(|i| source_matches(&session, i));
    let directory = source.map(|s| s.cwd.clone());
    let reason = if session.attachment.is_some() && source.is_none() {
        Some("host_unavailable")
    } else {
        resume_block_reason(session.provider == "codex", source)
    };
    let archive = archive_info(&session, source, &external::archiving::metadata(paths)?);
    let archived = archive["archived"] == true;
    let placement =
        catalog::placement::project(paths, std::slice::from_ref(&session))?["memberships"]
            [&session.id]
            .clone();
    let mut value = public_session(session);
    value["placement"] = placement;
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
            action:
                Some(SessionAction::AddToHerdr {
                    session,
                    expected_generation,
                    herdr_session,
                    herdr_socket,
                    space,
                    new_space,
                    name,
                }),
            ..
        } => {
            if let Ok(s) = catalog::resolve(paths, &session, &expected_generation)
                && s.provider == ProviderId::Gemini.as_str()
            {
                bail!(
                    "UNSUPPORTED_PROVIDER_OPERATION: {}",
                    crate::provider::gemini::NO_HERDR_ATTACH
                );
            }
            if let Ok(s) = catalog::resolve(paths, &session, &expected_generation)
                && s.provider == ProviderId::Antigravity.as_str()
            {
                bail!(
                    "UNSUPPORTED_PROVIDER_OPERATION: {}",
                    crate::provider::antigravity::NO_HERDR_ATTACH
                );
            }
            claude::attach::add_to_herdr(
                paths,
                claude::attach::Request {
                    session: &session,
                    expected_generation: &expected_generation,
                    herdr_session: &herdr_session,
                    herdr_socket: herdr_socket.as_deref(),
                    space: space.as_deref(),
                    new_space: new_space.as_deref(),
                    name: name.as_deref(),
                },
            )
            .await?
        }
        Command::Session {
            action: Some(SessionAction::RepairIdentity),
            ..
        } => repair_identity(paths)?,
        Command::Session {
            session,
            expected_generation,
            action: None,
        } => {
            session_details(
                paths,
                session
                    .as_deref()
                    .context("INVALID_ARGUMENT: --session is required")?,
                expected_generation
                    .as_deref()
                    .context("INVALID_ARGUMENT: --expected-generation is required")?,
            )
            .await?
        }
        Command::Attachment {
            session,
            expected_generation,
        } => attachment(paths, &session, &expected_generation)?,
        Command::Capabilities => capabilities(),
        Command::Placements => {
            inventory(paths).await?;
            catalog::placement::project(paths, &catalog::list(paths)?)?
        }
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
        Command::Hosted { command } => crate::provider::claude::hosted::api(paths, command).await?,
        Command::Observe {
            session,
            expected_generation,
        } => crate::messaging::observe_public(paths, &session, &expected_generation).await?,
        Command::State {
            watch,
            interval,
            archive_filter,
            cached,
        } => loop {
            print(if cached {
                state_cached(paths, archive_filter).await?
            } else {
                state_filtered(paths, archive_filter).await?
            })?;
            if !watch {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        },
        Command::RecoveryReconcile { message } => {
            crate::messaging::recovery::reconcile_recovery(paths, &message).await?
        }
        Command::RecoveryPlan => {
            let mut bytes = Vec::new();
            std::io::stdin().take(32769).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 32768, "INPUT_TOO_LARGE");
            crate::messaging::recovery::plan(
                paths,
                serde_json::from_slice(&bytes).context("INVALID_REQUEST")?,
            )
            .await?
        }
        Command::TurnTarget {
            session,
            expected_generation,
        } => crate::messaging::checkpoints::target(paths, &session, &expected_generation).await?,
        Command::SteerTarget {
            session,
            expected_generation,
        } => crate::messaging::steering_target(paths, &session, &expected_generation).await?,
        Command::Answer {
            session,
            expected_generation,
            request_id,
            answers,
        } => {
            let raw = match answers {
                Some(raw) => raw,
                None => {
                    let mut bytes = Vec::new();
                    std::io::stdin()
                        .take(16 * 1024 + 1)
                        .read_to_end(&mut bytes)?;
                    String::from_utf8(bytes).context("INVALID_ANSWERS")?
                }
            };
            crate::questions::answer(
                paths,
                crate::questions::AnswerRequest {
                    session,
                    expected_generation,
                    request_id,
                    answers: crate::questions::parse_answers(&raw)?,
                },
            )
            .await?
        }
        Command::Checkpoints => {
            let mut bytes = Vec::new();
            std::io::stdin().take(32769).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 32768, "INPUT_TOO_LARGE");
            crate::messaging::checkpoints::read(
                paths,
                serde_json::from_slice(&bytes).context("INVALID_REQUEST")?,
            )
            .await?
        }
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
            if let Some(provider) =
                native::of(&catalog::resolve(paths, &session, &expected_generation)?.provider)
            {
                // The same path as Codex: a HERDR-attached session renames its tab
                // (name_source herdr_last_observed); otherwise the catalog label.
                // Claude applies the label as --name on the next launch/resume; Gemini has no native title.
                let mut value = public_session(
                    catalog::names::rename(paths, &session, &expected_generation, &name).await?,
                );
                value["native_rename"] =
                    json!({"supported":false,"reason":native::label_only(provider)});
                value
            } else {
                public_session(
                    catalog::names::rename(paths, &session, &expected_generation, &name).await?,
                )
            }
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
        Command::Relocate {
            session,
            expected_generation,
            to,
            move_files,
            dry_run,
        } => {
            let (_, source) = selected(paths, &session, &expected_generation).await?;
            if dry_run {
                external::relocation::preview_snapshot(paths, &source, &to, move_files).await?
            } else {
                operation(
                    paths,
                    external::relocation::queue_public(
                        paths,
                        source,
                        &to,
                        move_files,
                        (&session, &expected_generation),
                    )
                    .await?,
                )?
            }
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
        } if catalog::resolve(paths, &session, &expected_generation)
            .is_ok_and(|s| s.provider == ProviderId::Antigravity.as_str()) =>
        {
            stop_antigravity(paths, &session, &expected_generation).await?
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
            space,
            new_space,
            name,
        } if catalog::resolve(paths, &session, &expected_generation)
            .is_ok_and(|s| native::of(&s.provider).is_some()) =>
        {
            let (s, observation) = native_selected(paths, &session, &expected_generation)?;
            native::ensure_resumable(&s, &observation)?;
            crate::messaging::headless::ensure_idle(paths, &s.conversation_id)?;
            let directory = native::resume_directory(&s, &observation)?;
            let s = named(paths, s, &expected_generation, name.as_deref())?;
            std::env::set_current_dir(&directory)?;
            external::hosting::launch_placed(
                paths,
                &herdr_session,
                &format!("{}:{}", s.provider, s.account),
                Some(&s.conversation_id),
                (&session, &s.generation()),
                resume_placement(space.as_deref(), new_space.as_deref(), &s),
            )
            .await?
        }
        Command::Resume {
            session,
            expected_generation,
            herdr_session,
            space,
            new_space,
            name,
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
            let directory = resume_cwd(&source.cwd)?;
            let s = named(paths, s, &expected_generation, name.as_deref())?;
            std::env::set_current_dir(&directory)?;
            external::hosting::launch_placed(
                paths,
                &herdr_session,
                &s.account,
                Some(&s.conversation_id),
                (&session, &s.generation()),
                resume_placement(space.as_deref(), new_space.as_deref(), &s),
            )
            .await?
        }
        Command::Spaces {
            session,
            expected_generation,
            herdr_session: Some(herdr_session),
            herdr_socket,
        } => external::spaces::host_list(
            paths,
            &session,
            &expected_generation,
            &herdr_session,
            herdr_socket.as_deref(),
        )?,
        Command::Spaces {
            session,
            expected_generation,
            ..
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
            ops.extend(
                crate::account_rename::list(paths)?
                    .iter()
                    .map(crate::account_rename::public),
            );
            ops.extend(external::archiving::operations(paths)?);
            ops.extend(external::spaces::operations(paths)?);
            ops.extend(crate::inline::operations(paths)?);
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
        Command::AbandonOperation { operation, confirm } => {
            if crate::account_rename::exists(paths, &operation) {
                crate::account_rename::abandon(paths, &operation, &confirm)?
            } else {
                crate::resume::abandon(paths, &operation, &confirm).await?
            }
        }
        Command::CancelOperation { operation: id } => {
            managed_operation(paths, &id)?;
            external::cancel_move(paths, &id)?;
            json!({"id":id,"cancellation_requested":true})
        }
        Command::Recover { operation: id } if external::spaces::exists(paths, &id) => {
            external::spaces::recover(paths, &id)?
        }
        Command::Recover { operation: id } if crate::account_rename::exists(paths, &id) => {
            daemon::ensure_started(paths).await?;
            daemon::call(
                paths,
                "account/rename-backend/recover",
                json!({"request_id":id}),
            )
            .await?
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
            crate::messaging::headless::dispatch(paths, &bytes).await?
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

/// Apply an optional Start Session name before launch (generation-checked).
/// Where `api resume` opens a session: the new tab (in the given Space, or a new
/// Space) carries the session's current name, already updated by `--name`. Live
/// HERDR session names follow the tab label, so a tab left with the default label
/// would rename the session.
fn resume_placement<'a>(
    space: Option<&'a str>,
    new_space: Option<&'a str>,
    s: &'a catalog::Session,
) -> external::hosting::Placement<'a> {
    external::hosting::Placement {
        space,
        new_space,
        label: Some(s.name.as_str()).filter(|n| !n.trim().is_empty()),
        strict: true,
    }
}
fn named(
    paths: &Paths,
    s: catalog::Session,
    generation: &str,
    name: Option<&str>,
) -> Result<catalog::Session> {
    match name {
        Some(name) if name != s.name => {
            crate::launch::validate_session_name(name)?;
            catalog::rename(paths, &s.id, generation, name)
        }
        _ => Ok(s),
    }
}

/// Stop one running Antigravity session: SIGTERM to its exact verified agy
/// process (see `provider::antigravity::stop`). Synchronous: the result is
/// final, with `phase:"complete"` only after the process exited.
async fn stop_antigravity(paths: &Paths, id: &str, generation: &str) -> Result<Value> {
    let (s, observation) = native_selected(paths, id, generation)?;
    ensure!(observation.availability == "running", "SESSION_NOT_RUNNING");
    let account = native::account(paths, ProviderId::Antigravity, &s.account)?;
    let _lock = crate::external::conversation_lock(paths, &s.conversation_id)?;
    let (home, conversation) = (account.home.clone(), s.conversation_id.clone());
    let stopped = tokio::task::spawn_blocking(move || {
        crate::provider::antigravity::stop(
            &home,
            &conversation,
            std::time::Duration::from_secs(crate::provider::antigravity::STOP_SECONDS),
        )
    })
    .await??;
    Ok(
        json!({"id":uuid::Uuid::new_v4().to_string(),"source_session_id":s.id,"kind":"terminate","phase":"complete",
        "detail":"Antigravity received SIGTERM, flushed its conversation and exited; the session is saved and can be resumed.",
        "provider":"antigravity","signal":"SIGTERM","pid":stopped.pid,"process_start":stopped.process_start,
        "provider_conversation_id":stopped.conversation_id,"source_account_id":format!("antigravity:{}",s.account)}),
    )
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
        "can_recover": crate::background::worker_stopped(&paths.root.join(crate::layout::HANDOFF_JOBS).join(format!("{}.worker-lock", j.id)))? && (j.phase == "failed" || (!matches!(j.phase.as_str(), "complete" | "cancelled" | "recovered") && daemon::now() - j.created_at > 10)),
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
    fn resume_cwd_requires_absolute_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            resume_cwd(tmp.path().to_str().unwrap()).unwrap(),
            tmp.path()
        );
        for bad in ["", "relative", "/nonexistent/sessionsation-dir"] {
            let e = resume_cwd(bad).unwrap_err().to_string();
            assert!(e.starts_with("SAVED_DIRECTORY_UNAVAILABLE"), "{e}");
        }
        let f = tmp.path().join("f");
        std::fs::write(&f, "").unwrap();
        assert!(resume_cwd(f.to_str().unwrap()).is_err());
    }
    #[test]
    fn attachment_lookup_is_read_only_and_checks_catalog_and_process_epochs() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let pid = std::process::id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let epoch: u64 = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        for host in ["herdr", "tmux"] {
            let s = catalog::record_launch(
                &p,
                "fixture",
                &uuid::Uuid::new_v4().to_string(),
                host,
                Some(catalog::Attachment {
                    host: host.into(),
                    target: format!("{host}/fixture/target"),
                    pid,
                    process_start: epoch,
                }),
            )
            .unwrap();
            let before = std::fs::read(
                p.root
                    .join(crate::layout::SESSION_CATALOG)
                    .join("catalog.json"),
            )
            .unwrap();
            let result = attachment(&p, &s.id, &s.generation()).unwrap();
            assert_eq!(result["id"], s.id);
            assert_eq!(result["attachment"]["host"], host);
            assert_eq!(result["starts_process"], false);
            assert_eq!(result["sends_input"], false);
            assert_eq!(
                std::fs::read(
                    p.root
                        .join(crate::layout::SESSION_CATALOG)
                        .join("catalog.json")
                )
                .unwrap(),
                before
            );
            catalog::rename(&p, &s.id, &s.generation(), &format!("{host}-renamed")).unwrap();
            assert!(
                attachment(&p, &s.id, &s.generation())
                    .unwrap_err()
                    .to_string()
                    .contains("STALE_TARGET")
            );
        }
        let s = catalog::record_launch(
            &p,
            "fixture",
            "wrong-epoch",
            "Wrong",
            Some(catalog::Attachment {
                host: "herdr".into(),
                target: "herdr/fixture/target".into(),
                pid,
                process_start: epoch + 1,
            }),
        )
        .unwrap();
        assert!(
            attachment(&p, &s.id, &s.generation())
                .unwrap_err()
                .to_string()
                .contains("SESSION_ATTACHMENT_UNAVAILABLE")
        );
        let s = catalog::record_launch(&p, "fixture", "saved", "Saved", None).unwrap();
        assert!(
            attachment(&p, &s.id, &s.generation())
                .unwrap_err()
                .to_string()
                .contains("SESSION_NOT_ATTACHED")
        );
        assert!(!p.root.join(crate::layout::LAUNCHES).exists());
    }
    #[test]
    fn resume_into_a_space_keeps_the_session_name_as_the_tab_label() {
        let s = catalog::Session {
            id: "stable".into(),
            name: "my-work".into(),
            provider: "codex".into(),
            account: "selected".into(),
            conversation_id: "thread".into(),
            revision: 1,
            attachment: None,
            directory: None,
            previous_conversation_ids: vec![],
        };
        // Without --name the existing name labels the tab (live HERDR names follow the
        // tab label), never the "sessionsation" default.
        for space in [Some("w1"), None] {
            let p = resume_placement(space, None, &s);
            assert_eq!(p.space, space);
            assert_eq!(p.label, Some("my-work"));
            assert!(p.strict);
        }
        let unnamed = catalog::Session {
            name: " ".into(),
            ..s
        };
        assert_eq!(resume_placement(Some("w1"), None, &unnamed).label, None);
    }
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
            directory: None,
            previous_conversation_ids: vec![],
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
            Some("unsupported_provider")
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

#[cfg(test)]
mod codex_reason_tests {
    use super::*;
    fn row(caps: Value) -> Value {
        json!({"capabilities":caps,"capability_reasons":{"add_to_herdr":"X"}})
    }
    #[test]
    fn every_false_codex_capability_has_a_reason() {
        let all = [
            "move_space",
            "resume",
            "change_directory",
            "move_project",
            "switch_account",
            "restart",
            "stop",
            "archive",
            "delete",
        ];
        let falses = |v: &Value| {
            let mut v = v.clone();
            for k in all {
                v["capabilities"][k] = json!(false);
            }
            v
        };
        let ctx = |running, unambiguous, known, in_herdr, archived| CodexReasonContext {
            running,
            unambiguous,
            known,
            in_herdr,
            resume_supported: true,
            archived,
        };
        let mut v = falses(&row(json!({})));
        fill_codex_reasons(&mut v, &ctx(false, true, true, false, false));
        for k in all {
            assert!(v["capability_reasons"][k].is_string(), "{k}: {v}");
        }
        assert_eq!(v["capability_reasons"]["stop"], "SESSION_NOT_RUNNING");
        assert_eq!(
            v["capability_reasons"]["add_to_herdr"], "X",
            "existing reasons are kept"
        );
        let mut v = falses(&row(json!({})));
        fill_codex_reasons(&mut v, &ctx(true, false, true, true, false));
        assert_eq!(v["capability_reasons"]["stop"], "AMBIGUOUS_SESSION");
        assert_eq!(
            v["capability_reasons"]["archive"],
            "SESSION_RUNNING_STOP_FIRST"
        );
        assert_eq!(v["capability_reasons"]["resume"], "SESSION_ALREADY_RUNNING");
        let mut v = falses(&row(json!({})));
        fill_codex_reasons(&mut v, &ctx(true, true, true, false, false));
        assert_eq!(v["capability_reasons"]["move_space"], "NOT_IN_HERDR");
        let mut v = falses(&row(json!({})));
        fill_codex_reasons(&mut v, &ctx(true, true, false, true, false));
        assert_eq!(v["capability_reasons"]["switch_account"], "ACCOUNT_UNKNOWN");
        let mut v = falses(&row(json!({})));
        fill_codex_reasons(&mut v, &ctx(false, true, true, false, true));
        assert_eq!(v["capability_reasons"]["restart"], "SESSION_ARCHIVED");
        // True capabilities get no reason.
        let mut v = row(json!({"stop":true}));
        fill_codex_reasons(&mut v, &ctx(true, true, true, true, false));
        assert!(v["capability_reasons"]["stop"].is_null());
    }
}

#[cfg(test)]
mod gemini_auth_failure_tests {
    use super::*;
    use crate::provider::gemini::auth_failure;
    #[test]
    fn a_failure_recorded_after_the_cached_state_shows_at_once_and_clears() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(t.path().join("root"))).unwrap();
        let (home, _) = crate::provider::gemini::create_home(&paths, "g1").unwrap();
        std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
        // A cached state written before the failing run.
        let row = json!({"name":"g1","provider":"gemini","enabled":true,"auth_required":false,"deletion_id":null,"auth_detail":null});
        let mut cached = json!({"accounts": project_accounts(&paths, vec![row]).unwrap()});
        assert_eq!(cached["accounts"][0]["selectable"], true);
        auth_failure::record(
            &paths,
            "g1",
            &home,
            auth_failure::tests::UNSUPPORTED_CLIENT_STDERR,
        )
        .unwrap();
        refresh_cached_accounts(&paths, &mut cached);
        let a = &cached["accounts"][0];
        assert_eq!(a["auth_failed"], true);
        assert_eq!(a["auth_required"], true);
        assert_eq!(a["auth_detail"], "AUTH_FAILED");
        assert_eq!(a["selectable"], false);
        assert_eq!(a["prefill"]["eligible"], false);
        // A later success clears it in the cached rows too.
        auth_failure::clear(&paths, "g1");
        refresh_cached_accounts(&paths, &mut cached);
        let a = &cached["accounts"][0];
        assert_eq!(a["auth_failed"], false);
        assert_eq!(a["auth_required"], false);
        assert!(a["auth_detail"].is_null());
        assert_eq!(a["selectable"], true);
        // A Gemini login problem other than the failure is kept.
        let mut other = json!({"accounts":[{"name":"g1","provider":"gemini","enabled":true,"auth_required":true,"deletion_id":null,"auth_detail":"AUTH_REQUIRED","membership":"enrolled"}]});
        refresh_cached_accounts(&paths, &mut other);
        assert_eq!(other["accounts"][0]["auth_required"], true);
        assert_eq!(other["accounts"][0]["selectable"], false);
    }
    #[test]
    fn recorded_gemini_auth_failure_makes_the_account_unselectable_until_cleared() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(t.path().join("root"))).unwrap();
        let (home, _) = crate::provider::gemini::create_home(&paths, "g1").unwrap();
        std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
        let row = || json!({"name":"g1","provider":"gemini","enabled":true,"auth_required":false,"deletion_id":null,"auth_detail":null});
        let project = || project_accounts(&paths, vec![row()]).unwrap().remove(0);
        let a = project();
        assert_eq!(a["auth_failed"], false);
        assert_eq!(a["selectable"], true);
        // A generic crash does not mark the account failed.
        auth_failure::record(&paths, "g1", &home, "panic: segmentation fault").unwrap();
        assert_eq!(project()["auth_failed"], false);
        auth_failure::record(&paths, "g1", &home, "Error: client no longer supported").unwrap();
        let a = project();
        assert_eq!(a["auth_failed"], true);
        assert!(
            a["auth_failure_reason"]
                .as_str()
                .unwrap()
                .starts_with("AUTH_CLIENT_UNSUPPORTED")
        );
        assert!(a["auth_failure_observed_at"].as_i64().is_some());
        assert_eq!(a["auth_required"], true);
        assert_eq!(a["selectable"], false);
        assert_eq!(a["prefill"]["eligible"], false);
        // Claude rows are untouched.
        let c = project_accounts(&paths, vec![json!({"name":"g1","provider":"claude","enabled":true,"auth_required":false,"deletion_id":null})]).unwrap().remove(0);
        assert!(c.get("auth_failed").is_none());
        auth_failure::clear(&paths, "g1");
        let a = project();
        assert_eq!(a["auth_failed"], false);
        assert_eq!(a["selectable"], true);
    }
}

#[cfg(test)]
mod contract_semver_tests {
    use super::*;
    #[test]
    fn api_version_negotiation_accepts_the_same_major_only() {
        for ok in ["1", "1.0", "1.25", "1.99.7", "1.25.0"] {
            assert_eq!(parse_api_version(ok), Ok(1), "{ok}");
        }
        for bad in ["0", "2", "2.0.0", "", "1.", "1.x", "1.2.3.4", "-1", "01x"] {
            assert!(parse_api_version(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn contract_version_matches_revision_and_parts() {
        assert_eq!(
            CONTRACT_VERSION,
            format!("{CONTRACT_MAJOR}.{CONTRACT_MINOR}.{CONTRACT_PATCH}")
        );
        let v = version().unwrap();
        assert_eq!(v["contract_version"], CONTRACT_VERSION);
        assert_eq!(v["contract_revision"], CONTRACT_MINOR);
        let c = capabilities();
        assert_eq!(c["contract_version"], CONTRACT_VERSION);
        assert_eq!(c["contract_revision"], CONTRACT_MINOR);
    }
}

#[cfg(test)]
mod terminal_resume_gate_tests {
    use super::ensure_resumable_in_terminal as gate;

    #[test]
    fn unfinished_history_needs_explicit_confirmation_and_nothing_else_passes() {
        assert!(gate(None, false).is_ok());
        let e = gate(Some("unfinished"), false).unwrap_err().to_string();
        assert!(
            e.contains("UNFINISHED_HISTORY_CONFIRMATION_REQUIRED"),
            "{e}"
        );
        assert!(gate(Some("unfinished"), true).is_ok());
        for reason in ["busy", "unknown", "diverged", "moving", "future"] {
            let e = gate(Some(reason), true).unwrap_err().to_string();
            assert!(e.contains("SESSION_STATE_UNVERIFIED"), "{e}");
        }
    }
}
