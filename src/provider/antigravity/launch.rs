//! Antigravity new/resume launches in the current terminal. A new session first
//! gets its conversation from agy (an empty headless run that sends no prompt),
//! the catalog entry is recorded, and then agy is exec'd in place on that
//! conversation, so the recorded PID/start time is the agy process itself (the
//! host wrapper execs the real binary without forking).
use super::super::claude::{Enrolled, Observation};
use crate::{
    catalog,
    config::Config,
    paths::Paths,
    provider::{ProviderId, antigravity},
};
use anyhow::{Context, Result, bail, ensure};
use std::{os::unix::process::CommandExt, path::Path};

pub use crate::provider::claude::launch::resume_directory;

fn context(paths: &Paths, name: &str) -> Result<(Enrolled, Config)> {
    let account = antigravity::account(paths, name)?;
    crate::accounts::ensure_available(paths, ProviderId::Antigravity, name)?;
    ensure!(account.enabled, "ACCOUNT_UNAVAILABLE: account is disabled");
    ensure!(
        account.home.is_dir(),
        "ACCOUNT_HOME_UNAVAILABLE: the Antigravity home does not exist"
    );
    Ok((account, crate::store::load_config(paths)?))
}

/// Resume preconditions shared by the CLI, launcher and HERDR resume paths.
pub fn ensure_resumable(session: &catalog::Session, observation: &Observation) -> Result<()> {
    ensure!(
        session.provider == ProviderId::Antigravity.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    match observation.availability {
        "running" => bail!("SESSION_ALREADY_RUNNING: already running; focus it"),
        "stale" => bail!(
            "SESSION_STALE: the Antigravity conversation for this session is missing from its account home"
        ),
        "stopped" => {}
        _ => bail!("SESSION_STATE_UNVERIFIED: the Antigravity session state could not be observed"),
    }
    ensure!(
        observation.attachment_switched
            || session.attachment.as_ref().is_none_or(|a| {
                crate::external::attachment_definitely_gone(a.pid, a.process_start)
            }),
        "SESSION_ALREADY_RUNNING: already running; focus it"
    );
    Ok(())
}

fn exec(config: &Config, home: &Path, cwd: &Path, argv: &[String]) -> Result<()> {
    if config.antigravity_pretrust_folders && argv.iter().any(|a| a == antigravity::YOLO_FLAG) {
        antigravity::trust::pretrust_before_start(home, cwd);
    }
    let mut command = antigravity::command(config, home);
    command.args(&argv[1..]).current_dir(cwd);
    Err(command.exec()).context("could not launch Antigravity CLI")
}

/// Start a new Antigravity session in this terminal. `creation` is an admitted
/// API launch (request id, reserved name) completed instead of a fresh entry.
pub async fn exec_new(
    paths: &Paths,
    account: &str,
    label: Option<&str>,
    creation: Option<&str>,
) -> Result<()> {
    let (account, config) = context(paths, account)?;
    let cwd = std::env::current_dir()?;
    ensure!(cwd.is_dir(), "working directory is not a directory");
    let directory = cwd.to_str().context("working directory is not UTF-8")?;
    antigravity::prelaunch_gate(&account.home)?;
    let id = {
        let (config, home, cwd) = (config.clone(), account.home.clone(), cwd.clone());
        tokio::task::spawn_blocking(move || antigravity::allocate(&config, &home, &cwd)).await??
    };
    let attachment = crate::external::current_attachment(paths)?;
    match creation {
        Some(request) => {
            crate::launch::attached(paths, request, &id, attachment)?;
        }
        None => {
            catalog::record_provider_launch(
                paths,
                ProviderId::Antigravity,
                &account.name,
                &id,
                label
                    .or_else(|| cwd.file_name().and_then(|n| n.to_str()))
                    .unwrap_or("antigravity"),
                Some(directory),
                attachment,
            )?;
        }
    }
    exec(
        &config,
        &account.home,
        &cwd,
        &antigravity::open_argv(&config, &id, config.effective_launch_permissions()),
    )
}

/// Resume a stopped Antigravity session in its recorded directory, in this terminal.
pub async fn exec_resume(paths: &Paths, id: &str, generation: &str) -> Result<()> {
    let session = catalog::resolve(paths, id, generation)?;
    ensure!(
        session.provider == ProviderId::Antigravity.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    let (account, config) = context(paths, &session.account)?;
    crate::lifecycle::ensure_available(paths, &session.conversation_id, None)?;
    let _lock = crate::external::conversation_lock(paths, &session.conversation_id)?;
    crate::messaging::headless::ensure_idle(paths, &session.conversation_id)?;
    let observation = antigravity::observe(
        &account.home,
        &session.conversation_id,
        session.attachment.as_ref(),
    );
    ensure_resumable(&session, &observation)?;
    let directory = resume_directory(&session, &observation)?;
    antigravity::prelaunch_gate(&account.home)?;
    let attachment = crate::external::current_attachment(paths)?;
    let current = catalog::move_attachment(paths, &session, attachment)?;
    exec(
        &config,
        &account.home,
        Path::new(&directory),
        &antigravity::open_argv(
            &config,
            &current.conversation_id,
            config.effective_launch_permissions(),
        ),
    )
}
