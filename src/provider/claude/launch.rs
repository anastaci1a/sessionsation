//! Claude new/resume launches in the current terminal. The catalog entry (with
//! the native session id chosen here) is recorded before Claude is exec'd in
//! place, so the recorded PID/start time is the Claude process itself.
use super::{Enrolled, Observation};
use crate::{
    catalog,
    config::Config,
    paths::Paths,
    provider::{ProviderId, claude},
};
use anyhow::{Context, Result, bail, ensure};
use std::{os::unix::process::CommandExt, path::Path};

fn context(paths: &Paths, name: &str) -> Result<(Enrolled, Config)> {
    let account = claude::account(paths, name)?;
    ensure!(account.enabled, "ACCOUNT_UNAVAILABLE: account is disabled");
    ensure!(
        account.home.is_dir(),
        "ACCOUNT_HOME_UNAVAILABLE: the Claude config home does not exist"
    );
    Ok((account, crate::store::load_config(paths)?))
}

/// Resume preconditions shared by the CLI, launcher and HERDR resume paths.
pub fn ensure_resumable(session: &catalog::Session, observation: &Observation) -> Result<()> {
    ensure!(
        session.provider == ProviderId::Claude.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    match observation.availability {
        "running" => bail!("SESSION_ALREADY_RUNNING: already running; focus it"),
        "stale" => bail!(
            "SESSION_STALE: the Claude transcript for this session is missing from its account config home"
        ),
        "stopped" => {}
        _ => bail!("SESSION_STATE_UNVERIFIED: the Claude session state could not be observed"),
    }
    ensure!(
        session
            .attachment
            .as_ref()
            .is_none_or(|a| crate::external::attachment_definitely_gone(a.pid, a.process_start)),
        "SESSION_ALREADY_RUNNING: already running; focus it"
    );
    Ok(())
}
/// The directory a stopped session resumes in: catalog record, then transcript.
pub fn resume_directory(session: &catalog::Session, observation: &Observation) -> Result<String> {
    let directory = session
        .directory
        .clone()
        .or_else(|| observation.directory.clone())
        .context("SAVED_DIRECTORY_UNAVAILABLE")?;
    ensure!(
        Path::new(&directory).is_absolute() && Path::new(&directory).is_dir(),
        "SAVED_DIRECTORY_UNAVAILABLE"
    );
    Ok(directory)
}

fn exec(config: &Config, home: &Path, cwd: &Path, argv: &[String]) -> Result<()> {
    let mut command = claude::command(config, home);
    command.args(&argv[1..]).current_dir(cwd);
    Err(command.exec()).context("could not launch Claude")
}

/// Start a new Claude session in this terminal. `creation` is an admitted API
/// launch (request id, reserved name) completed instead of a fresh catalog entry.
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
    claude::prelaunch_gate(&config, &account.home).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let attachment = crate::external::current_attachment(paths)?;
    let session = match creation {
        Some(request) => crate::launch::attached(paths, request, &id, attachment)?,
        None => catalog::record_provider_launch(
            paths,
            ProviderId::Claude,
            &account.name,
            &id,
            label
                .or_else(|| cwd.file_name().and_then(|n| n.to_str()))
                .unwrap_or("claude"),
            Some(directory),
            attachment,
        )?,
    };
    let settings = claude::launch_settings(&std::env::current_exe()?);
    let argv = claude::new_argv(&config, &id, &session.name, &settings);
    exec(&config, &account.home, &cwd, &argv)
}

/// Resume a stopped Claude session in its recorded directory, in this terminal.
pub async fn exec_resume(paths: &Paths, id: &str, generation: &str) -> Result<()> {
    let session = catalog::resolve(paths, id, generation)?;
    ensure!(
        session.provider == ProviderId::Claude.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    let (account, config) = context(paths, &session.account)?;
    crate::lifecycle::ensure_available(paths, &session.conversation_id, None)?;
    let _lock = crate::external::conversation_lock(paths, &session.conversation_id)?;
    let observation = claude::observe(
        &account.home,
        &session.conversation_id,
        session.attachment.as_ref(),
    );
    ensure_resumable(&session, &observation)?;
    let directory = resume_directory(&session, &observation)?;
    claude::prelaunch_gate(&config, &account.home).await?;
    let attachment = crate::external::current_attachment(paths)?;
    let current = catalog::move_attachment(paths, &session, attachment)?;
    let settings = claude::launch_settings(&std::env::current_exe()?);
    let argv = claude::resume_argv(&config, &current.conversation_id, &current.name, &settings);
    exec(&config, &account.home, Path::new(&directory), &argv)
}
