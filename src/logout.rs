//! Account log out for every provider, using each tool's own mechanism. The
//! supervisor (sole credential owner) calls this after the account is verified
//! idle; nothing here reads credential contents.
use crate::{
    config::Config,
    paths::Paths,
    provider::{ProviderId, claude, gemini},
    rpc::Rpc,
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{path::Path, process::Stdio, time::Duration};

pub const CODEX_METHOD: &str = "codex app-server account/logout (running helper or account server); `codex logout` when none runs";
pub const CLAUDE_METHOD: &str = "claude auth logout";
pub const GEMINI_METHOD: &str = "gemini-cli 0.66.0 clearCachedCredentialFile: remove .gemini/oauth_creds.json and clear the active account in .gemini/google_accounts.json (file storage; gemini has no logout command)";
const TIMEOUT: Duration = Duration::from_secs(30);

pub fn method(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Codex => CODEX_METHOD,
        ProviderId::Claude => CLAUDE_METHOD,
        ProviderId::Gemini => GEMINI_METHOD,
    }
}

fn in_use(detail: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("ACCOUNT_IN_USE: log out refused while the account is in use ({detail})")
}

/// Refuse while any live session or process of the provider uses the account.
pub async fn ensure_idle(
    paths: &Paths,
    provider: ProviderId,
    name: &str,
    home: &Path,
) -> Result<()> {
    if provider == ProviderId::Codex {
        if let Ok(lease) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(home.join(".switcher-lease"))
        {
            use fs2::FileExt;
            if lease.try_lock_exclusive().is_err() {
                return Err(in_use("ACCOUNT_LEASE_HELD"));
            }
            let _ = FileExt::unlock(&lease);
        }
        crate::account_deletion::idle(paths, home)
            .await
            .map_err(in_use)
    } else {
        crate::account_deletion::native_idle(paths, provider, name, home).map_err(in_use)
    }
}

async fn run(mut command: tokio::process::Command, what: &str) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(TIMEOUT, command.status())
        .await
        .with_context(|| format!("LOGOUT_TIMEOUT: {what} did not finish"))?
        .with_context(|| format!("LOGOUT_FAILED: {what} could not start"))?;
    ensure!(
        status.success(),
        "LOGOUT_FAILED: {what} exited unsuccessfully"
    );
    Ok(())
}

/// Codex: the account's own app-server clears the stored auth and its in-memory
/// copy (`account/logout`). With no helper, a running account server is asked;
/// with neither, the official `codex logout` runs with the account's CODEX_HOME.
pub async fn codex(config: &Config, home: &Path, helper: Option<&Rpc>) -> Result<()> {
    if let Some(helper) = helper {
        helper.request("account/logout", json!({})).await?;
        let read = helper
            .request("account/read", json!({"refreshToken":false}))
            .await?;
        ensure!(
            read["account"].is_null(),
            "LOGOUT_INCOMPLETE: Codex still reports a signed-in account"
        );
        return Ok(());
    }
    if crate::control::available(home) {
        crate::control::Control::connect(home)
            .await?
            .request("account/logout", json!({}))
            .await?;
    } else {
        let mut command = tokio::process::Command::new(&config.codex_binary);
        command
            .arg("logout")
            .env("CODEX_HOME", home)
            .current_dir(home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("CODEX_ACCESS_TOKEN");
        run(command, "codex logout").await?;
    }
    ensure!(
        matches!(std::fs::symlink_metadata(home.join("auth.json")), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        "LOGOUT_INCOMPLETE: Codex credentials are still stored"
    );
    Ok(())
}

/// Claude: `claude auth logout` with the account environment, then confirmed
/// with `claude auth status`.
pub async fn claude(config: &Config, home: &Path) -> Result<()> {
    let mut command = tokio::process::Command::from(claude::command(config, home));
    command.args(["auth", "logout"]).current_dir(home);
    run(command, "claude auth logout").await?;
    let status = claude::auth_status(config, home).await?;
    ensure!(
        !status.logged_in,
        "LOGOUT_INCOMPLETE: Claude still reports a login (an API key or cloud provider environment is not cleared by `claude auth logout`)"
    );
    Ok(())
}

/// Gemini: see `gemini::logout`.
pub fn gemini(home: &Path) -> Result<()> {
    gemini::logout(home)?;
    ensure!(
        !gemini::auth_status(home).logged_in,
        "LOGOUT_INCOMPLETE: Gemini credentials are still present"
    );
    Ok(())
}
