//! Gemini new/resume launches in the current terminal. The catalog entry (with
//! the native session id chosen here) is recorded before Gemini is exec'd in
//! place, so the recorded PID/start time is the Gemini process itself.
use super::super::claude::{Enrolled, Observation};
use crate::{
    catalog,
    config::{Config, LaunchPermissions},
    paths::Paths,
    provider::{ProviderId, gemini},
};
use anyhow::{Context, Result, bail, ensure};
use std::{os::unix::process::CommandExt, path::Path};

pub use crate::provider::claude::launch::resume_directory;

fn context(paths: &Paths, name: &str) -> Result<(Enrolled, Config)> {
    let account = gemini::account(paths, name)?;
    crate::accounts::ensure_available(paths, ProviderId::Gemini, name)?;
    ensure!(account.enabled, "ACCOUNT_UNAVAILABLE: account is disabled");
    ensure!(
        account.home.is_dir(),
        "ACCOUNT_HOME_UNAVAILABLE: the Gemini home does not exist"
    );
    Ok((account, crate::store::load_config(paths)?))
}

/// Resume preconditions shared by the CLI, launcher and HERDR resume paths.
pub fn ensure_resumable(session: &catalog::Session, observation: &Observation) -> Result<()> {
    ensure!(
        session.provider == ProviderId::Gemini.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    match observation.availability {
        "running" => bail!("SESSION_ALREADY_RUNNING: already running; focus it"),
        "stale" => bail!(
            "SESSION_STALE: the Gemini chat file for this session is missing from its account home"
        ),
        "stopped" => {}
        _ => bail!("SESSION_STATE_UNVERIFIED: the Gemini session state could not be observed"),
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

/// Gemini approval flags for a start: `yolo` auto-approves every tool call;
/// `default` passes nothing (the account's own settings decide).
pub fn approval_args(mode: LaunchPermissions) -> Vec<String> {
    match mode {
        LaunchPermissions::Yolo => vec!["--approval-mode".into(), "yolo".into()],
        LaunchPermissions::Default => vec![],
    }
}
pub fn new_argv(config: &Config, session_id: &str, mode: LaunchPermissions) -> Vec<String> {
    let mut argv: Vec<String> = [config.gemini_binary.as_str(), "--session-id", session_id]
        .map(str::to_owned)
        .to_vec();
    argv.extend(approval_args(mode));
    argv
}
pub fn resume_argv(config: &Config, session_id: &str, mode: LaunchPermissions) -> Vec<String> {
    let mut argv: Vec<String> = [config.gemini_binary.as_str(), "-r", session_id]
        .map(str::to_owned)
        .to_vec();
    argv.extend(approval_args(mode));
    argv
}

fn exec(config: &Config, home: &Path, cwd: &Path, argv: &[String]) -> Result<()> {
    let mut command = gemini::command(config, home);
    command.args(&argv[1..]).current_dir(cwd);
    Err(command.exec()).context("could not launch Gemini")
}

/// Start a new Gemini session in this terminal. `creation` is an admitted API
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
    gemini::prelaunch_gate(&account.home)?;
    let id = uuid::Uuid::new_v4().to_string();
    let attachment = crate::external::current_attachment(paths)?;
    match creation {
        Some(request) => {
            crate::launch::attached(paths, request, &id, attachment)?;
        }
        None => {
            catalog::record_provider_launch(
                paths,
                ProviderId::Gemini,
                &account.name,
                &id,
                label
                    .or_else(|| cwd.file_name().and_then(|n| n.to_str()))
                    .unwrap_or("gemini"),
                Some(directory),
                attachment,
            )?;
        }
    }
    exec(
        &config,
        &account.home,
        &cwd,
        &new_argv(&config, &id, config.effective_launch_permissions()),
    )
}

/// Resume a stopped Gemini session in its recorded directory, in this terminal.
pub async fn exec_resume(paths: &Paths, id: &str, generation: &str) -> Result<()> {
    let session = catalog::resolve(paths, id, generation)?;
    ensure!(
        session.provider == ProviderId::Gemini.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    let (account, config) = context(paths, &session.account)?;
    crate::lifecycle::ensure_available(paths, &session.conversation_id, None)?;
    let _lock = crate::external::conversation_lock(paths, &session.conversation_id)?;
    crate::messaging::headless::ensure_idle(paths, &session.conversation_id)?;
    let observation = gemini::observe(
        &account.home,
        &session.conversation_id,
        session.attachment.as_ref(),
    );
    ensure_resumable(&session, &observation)?;
    let directory = resume_directory(&session, &observation)?;
    gemini::prelaunch_gate(&account.home)?;
    let attachment = crate::external::current_attachment(paths)?;
    let current = catalog::move_attachment(paths, &session, attachment)?;
    crate::inline::mark_attached(paths);
    exec(
        &config,
        &account.home,
        Path::new(&directory),
        &resume_argv(
            &config,
            &current.conversation_id,
            config.effective_launch_permissions(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn yolo_starts_pass_the_yolo_approval_mode_and_default_passes_none() {
        let config = Config::default();
        let id = "11111111-1111-4111-8111-111111111111";
        assert_eq!(
            new_argv(&config, id, LaunchPermissions::Yolo),
            ["gemini", "--session-id", id, "--approval-mode", "yolo"]
        );
        assert_eq!(
            resume_argv(&config, id, LaunchPermissions::Yolo),
            ["gemini", "-r", id, "--approval-mode", "yolo"]
        );
        assert_eq!(
            new_argv(&config, id, LaunchPermissions::Default),
            ["gemini", "--session-id", id]
        );
        assert_eq!(
            resume_argv(&config, id, LaunchPermissions::Default),
            ["gemini", "-r", id]
        );
    }
}
