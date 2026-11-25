//! Procedural provider dispatch moved behind the provider boundary in centralization step
//! S1 (guard role `provider-ops`): each function is the `match` its caller used to hold,
//! unchanged. Decisions that need no code live in [`super::ops::Facts`].
use super::{ProviderId, antigravity, claude, gemini};
use crate::{config::Config, paths::Paths};
use anyhow::Result;

/// Start a journaled inline new session in this process (`inline::run_new`): Codex through
/// the supervisor's account launch, native providers by exec.
pub async fn exec_new_inline(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
    id: &str,
    name: &str,
    options: &[String],
) -> Result<()> {
    start_new(paths, provider, account, id, name, options, None).await
}

/// Start a journaled new session in this process (`launch::run`, moved from
/// `launch::start_provider`): Codex through the supervisor's account launch with its
/// options, hosted Claude (`hosted` = its permission mode) in this terminal, native
/// providers by exec.
pub async fn start_new(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
    id: &str,
    name: &str,
    options: &[String],
    hosted: Option<Option<String>>,
) -> Result<()> {
    match provider {
        ProviderId::Codex => {
            crate::external::run_account_new_named_options(paths, account, id, name, options).await
        }
        ProviderId::Claude if hosted.is_some() => {
            claude::hosted::run_in_terminal(paths, account, None, hosted.flatten(), Some(id)).await
        }
        ProviderId::Claude => claude::launch::exec_new(paths, account, None, Some(id)).await,
        ProviderId::Gemini => gemini::launch::exec_new(paths, account, None, Some(id)).await,
        ProviderId::Antigravity => {
            antigravity::launch::exec_new(paths, account, None, Some(id)).await
        }
    }
}

/// Exec the official interactive login for a native provider in this terminal (moved from
/// `main.rs`). Returns only on failure to start it.
pub fn exec_login(
    paths: &Paths,
    name: &str,
    config: &Config,
    provider: ProviderId,
    home: &std::path::Path,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    match provider {
        ProviderId::Gemini => {
            use super::gemini::auth_mode;
            anyhow::ensure!(
                auth_mode::mode(home) != auth_mode::Mode::ApiKey,
                "AUTH_MODE_API_KEY: gemini:{name} uses an API key, which needs no login; store a key with `sessionsation account auth gemini:{name} --mode api-key --key-stdin`, or switch to Google login with `--mode google` first"
            );
            let mut command = super::gemini::login_command(config, home);
            command.current_dir(super::gemini::login_directory(home));
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                // Gemini continues into its chat after sign-in: supervise it and
                // end the login as soon as the credentials exist.
                let home = home.to_path_buf();
                let before = super::gemini::credentials_stamp(&home);
                let (ok, output) = crate::login::supervise_capture(command, {
                    let home = home.clone();
                    move || super::gemini::login_completed(&home, before)
                })?;
                // Clear a recorded failure on success; record a recognizable one (for
                // example Google rejecting the client) when the login did not complete.
                if ok {
                    super::gemini::auth_failure::clear(paths, name);
                    return Ok(());
                }
                let _ = super::gemini::auth_failure::record(paths, name, &home, &output);
                anyhow::bail!("Login did not complete: the CLI exited before sign-in finished");
            }
            Err(anyhow::Error::new(command.exec()).context("could not start gemini login"))
        }
        ProviderId::Antigravity => {
            // agy shows "Select login method" (Google OAuth), prints the Google URL
            // and reads the pasted code; the user completes it, nothing is automated.
            let mut command = super::antigravity::login_command(config, home);
            command.current_dir(super::antigravity::login_directory(home));
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                // agy continues into its chat after sign-in: supervise it and end
                // the login as soon as the token exists.
                let home = home.to_path_buf();
                let before = super::antigravity::credentials_stamp(&home);
                let (ok, _) = crate::login::supervise_capture(command, {
                    let home = home.clone();
                    move || super::antigravity::login_completed(&home, before)
                })?;
                anyhow::ensure!(
                    ok,
                    "Login did not complete: the CLI exited before sign-in finished"
                );
                return Ok(());
            }
            Err(anyhow::Error::new(command.exec()).context("could not start agy login"))
        }
        ProviderId::Claude | ProviderId::Codex => {
            let mut command = super::claude::command(config, home);
            command
                .args(&super::claude::login_argv(config)[1..])
                .current_dir(home);
            Err(anyhow::Error::new(command.exec()).context("could not start claude auth login"))
        }
    }
}
