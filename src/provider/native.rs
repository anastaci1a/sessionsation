//! Dispatch for providers whose sessions are native files inside an account
//! home (Claude Code, Gemini CLI and Antigravity CLI). They share the
//! observation, catalog and delivery model; everything that differs is chosen
//! here by `ProviderId`, so callers never compare provider strings.
use super::{
    ProviderId, antigravity,
    claude::{self, Enrolled, Live, Observation, Transcript},
    gemini,
};
use crate::{catalog, config::Config, paths::Paths};
use anyhow::{Result, bail};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const NATIVE: [ProviderId; 3] = [
    ProviderId::Claude,
    ProviderId::Gemini,
    ProviderId::Antigravity,
];

pub fn is_native(provider: ProviderId) -> bool {
    NATIVE.contains(&provider)
}
/// The native provider named by a catalog `provider` string, if any.
pub fn of(provider: &str) -> Option<ProviderId> {
    ProviderId::parse(provider).ok().filter(|p| is_native(*p))
}
pub fn require(provider: ProviderId) -> Result<ProviderId> {
    if !is_native(provider) {
        bail!("UNSUPPORTED_PROVIDER_OPERATION: {provider} accounts do not support this operation");
    }
    Ok(provider)
}
/// Display name used in messages.
pub fn title(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Claude => "Claude",
        ProviderId::Gemini => "Gemini",
        ProviderId::Antigravity => "Antigravity",
        ProviderId::Codex => "Codex",
    }
}
pub fn no_input_api(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Gemini => gemini::NO_INPUT_API,
        ProviderId::Antigravity => antigravity::NO_INPUT_API,
        ProviderId::Claude | ProviderId::Codex => claude::NO_INPUT_API,
    }
}
pub fn label_only(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Gemini => gemini::LABEL_ONLY,
        ProviderId::Antigravity => antigravity::LABEL_ONLY,
        ProviderId::Claude | ProviderId::Codex => claude::LABEL_ONLY,
    }
}
pub fn stripped_env(provider: ProviderId) -> &'static [&'static str] {
    match provider {
        ProviderId::Gemini => gemini::STRIPPED_ENV,
        ProviderId::Antigravity => antigravity::STRIPPED_ENV,
        ProviderId::Claude | ProviderId::Codex => claude::STRIPPED_ENV,
    }
}
/// Directory under the data root that holds managed homes.
pub fn managed_parent(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Gemini => "gemini-accounts",
        ProviderId::Antigravity => antigravity::MANAGED_PARENT,
        ProviderId::Claude | ProviderId::Codex => "claude-accounts",
    }
}
pub fn default_home(paths: &Paths, provider: ProviderId, name: &str) -> Result<PathBuf> {
    match provider {
        ProviderId::Gemini => gemini::home(paths, name),
        ProviderId::Antigravity => antigravity::home(paths, name),
        ProviderId::Claude | ProviderId::Codex => claude::default_home(paths, name),
    }
}

pub fn account(paths: &Paths, provider: ProviderId, name: &str) -> Result<Enrolled> {
    match provider {
        ProviderId::Gemini => gemini::account(paths, name),
        ProviderId::Antigravity => antigravity::account(paths, name),
        ProviderId::Claude | ProviderId::Codex => claude::account(paths, name),
    }
}
pub fn deleted(paths: &Paths, provider: ProviderId, name: &str) -> Result<bool> {
    match provider {
        ProviderId::Gemini => gemini::deleted(paths, name),
        ProviderId::Antigravity => antigravity::deleted(paths, name),
        ProviderId::Claude | ProviderId::Codex => claude::deleted(paths, name),
    }
}
pub fn command(provider: ProviderId, config: &Config, home: &Path) -> std::process::Command {
    match provider {
        ProviderId::Gemini => gemini::command(config, home),
        ProviderId::Antigravity => antigravity::command(config, home),
        ProviderId::Claude | ProviderId::Codex => claude::command(config, home),
    }
}
/// Bounded authentication probe: (problem code, non-identifying auth mode).
pub async fn probe(
    paths: &Paths,
    name: &str,
    provider: ProviderId,
    config: &Config,
    home: &Path,
) -> (Option<String>, Value) {
    match provider {
        ProviderId::Gemini => {
            let s = gemini::auth_status(home);
            // Credentials can be present yet rejected by the provider: the last observed
            // recognizable auth failure keeps the account in need of a login.
            let problem = s.problem().map(str::to_owned).or_else(|| {
                gemini::auth_failure::current(paths, name, home).map(|_| "AUTH_FAILED".to_owned())
            });
            (problem, s.mode())
        }
        ProviderId::Antigravity => {
            let s = antigravity::auth_status(home);
            (s.problem().map(str::to_owned), s.mode())
        }
        ProviderId::Claude | ProviderId::Codex => match claude::auth_status(config, home).await {
            Ok(s) => (
                s.problem().map(str::to_owned),
                serde_json::json!({"logged_in":s.logged_in,"auth_method":s.auth_method,"api_provider":s.api_provider,"subscription_type":s.subscription_type}),
            ),
            Err(e) => (
                Some(
                    e.to_string()
                        .split(':')
                        .next()
                        .unwrap_or("AUTH_STATUS_UNAVAILABLE")
                        .to_owned(),
                ),
                Value::Null,
            ),
        },
    }
}
pub async fn auth_report(provider: ProviderId, config: &Config, home: &Path) -> Value {
    match provider {
        ProviderId::Gemini => gemini::auth_report(home),
        ProviderId::Antigravity => antigravity::auth_report(home),
        ProviderId::Claude | ProviderId::Codex => claude::auth_report(config, home).await,
    }
}
/// Auth gate (and, for Gemini, settings seeding) before any launch or delivery.
pub async fn prelaunch_gate(provider: ProviderId, config: &Config, home: &Path) -> Result<()> {
    match provider {
        ProviderId::Gemini => gemini::prelaunch_gate(home),
        ProviderId::Antigravity => antigravity::prelaunch_gate(home),
        ProviderId::Claude | ProviderId::Codex => claude::prelaunch_gate(config, home).await,
    }
}
pub fn observe(
    provider: ProviderId,
    home: &Path,
    id: &str,
    recorded: Option<&catalog::Attachment>,
) -> Observation {
    match provider {
        ProviderId::Gemini => gemini::observe(home, id, recorded),
        ProviderId::Antigravity => antigravity::observe(home, id, recorded),
        ProviderId::Claude | ProviderId::Codex => claude::observe(home, id, recorded),
    }
}
/// Per-session `add_to_herdr` capability and reason.
pub fn add_to_herdr_capability(
    provider: ProviderId,
    session: &catalog::Session,
    observation: &Observation,
) -> (bool, Option<&'static str>) {
    let no_attach = match provider {
        ProviderId::Gemini => gemini::NO_HERDR_ATTACH,
        ProviderId::Antigravity => antigravity::NO_HERDR_ATTACH,
        ProviderId::Claude | ProviderId::Codex => {
            return claude::attach::capability(session, observation);
        }
    };
    match claude::attach::capability(session, observation) {
        // Not running / already in HERDR read the same as for Claude; a
        // running session elsewhere has no supported attach.
        (_, Some(reason))
            if reason.starts_with("SESSION_NOT_RUNNING")
                || reason.starts_with("ALREADY_IN_HERDR") =>
        {
            (false, Some(reason))
        }
        _ => (false, Some(no_attach)),
    }
}
pub fn find_transcript(provider: ProviderId, home: &Path, id: &str) -> Option<PathBuf> {
    match provider {
        ProviderId::Gemini => gemini::find_transcript(home, id),
        ProviderId::Antigravity => antigravity::find_transcript(home, id),
        ProviderId::Claude | ProviderId::Codex => claude::find_transcript(home, id),
    }
}
pub fn transcripts(provider: ProviderId, home: &Path) -> Vec<Transcript> {
    match provider {
        ProviderId::Gemini => gemini::transcripts(home),
        ProviderId::Antigravity => antigravity::transcripts(home),
        ProviderId::Claude | ProviderId::Codex => claude::transcripts(home),
    }
}
pub fn live_sessions(provider: ProviderId, home: &Path) -> Result<Vec<Live>> {
    match provider {
        ProviderId::Gemini => gemini::live_sessions(home),
        ProviderId::Antigravity => antigravity::live_sessions(home),
        ProviderId::Claude | ProviderId::Codex => claude::live_sessions(home),
    }
}
/// A provider process (any mode) using this home.
pub fn env_in_use(provider: ProviderId, config: &Config, home: &Path) -> bool {
    match provider {
        ProviderId::Gemini => gemini::env_in_use(&[home]),
        ProviderId::Antigravity => antigravity::env_in_use(&[home]),
        ProviderId::Claude | ProviderId::Codex => {
            claude::env_in_use(&[home], &config.claude_binary)
        }
    }
}
pub fn launch_observed(session: &catalog::Session) -> bool {
    match of(&session.provider) {
        Some(ProviderId::Gemini) => gemini::launch_observed(session),
        Some(ProviderId::Antigravity) => antigravity::launch_observed(session),
        Some(_) => claude::launch_observed(session),
        None => false,
    }
}
pub fn ensure_resumable(session: &catalog::Session, observation: &Observation) -> Result<()> {
    match of(&session.provider) {
        Some(ProviderId::Gemini) => gemini::launch::ensure_resumable(session, observation),
        Some(ProviderId::Antigravity) => {
            antigravity::launch::ensure_resumable(session, observation)
        }
        _ => claude::launch::ensure_resumable(session, observation),
    }
}
pub use claude::launch::resume_directory;
pub async fn exec_new(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
    label: Option<&str>,
    creation: Option<&str>,
) -> Result<()> {
    match provider {
        ProviderId::Gemini => gemini::launch::exec_new(paths, account, label, creation).await,
        ProviderId::Antigravity => {
            antigravity::launch::exec_new(paths, account, label, creation).await
        }
        ProviderId::Claude | ProviderId::Codex => {
            claude::launch::exec_new(paths, account, label, creation).await
        }
    }
}
pub async fn exec_resume(
    paths: &Paths,
    provider: ProviderId,
    id: &str,
    generation: &str,
) -> Result<()> {
    match provider {
        ProviderId::Gemini => gemini::launch::exec_resume(paths, id, generation).await,
        ProviderId::Antigravity => antigravity::launch::exec_resume(paths, id, generation).await,
        ProviderId::Claude | ProviderId::Codex => {
            claude::launch::exec_resume(paths, id, generation).await
        }
    }
}
/// Resume dispatch for a catalog session of any native provider.
pub async fn exec_resume_session(paths: &Paths, id: &str, generation: &str) -> Result<()> {
    let provider = of(&catalog::resolve(paths, id, generation)?.provider)
        .ok_or_else(|| anyhow::anyhow!("UNSUPPORTED_PROVIDER_OPERATION"))?;
    exec_resume(paths, provider, id, generation).await
}

/// Observations for every native provider, keyed by catalog session id. A
/// provider whose scan fails contributes nothing, as a single provider did before.
pub fn reconcile(paths: &Paths) -> Result<BTreeMap<String, (Observation, PathBuf)>> {
    let mut all = claude::reconcile(paths).unwrap_or_default();
    all.extend(gemini::reconcile(paths).unwrap_or_default());
    all.extend(antigravity::reconcile(paths).unwrap_or_default());
    Ok(all)
}

#[cfg(test)]
mod gemini_probe_tests {
    use super::*;
    #[tokio::test]
    async fn probe_reports_a_recorded_gemini_auth_failure_despite_present_credentials() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(t.path().join("root"))).unwrap();
        let home = paths.root.join("gemini-accounts/g1");
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
        std::fs::write(
            home.join(".gemini/google_accounts.json"),
            br#"{"active":"a@example.invalid"}"#,
        )
        .unwrap();
        let config = Config::default();
        let (problem, _) = probe(&paths, "g1", ProviderId::Gemini, &config, &home).await;
        assert_eq!(problem, None);
        gemini::auth_failure::record(&paths, "g1", &home, "invalid_grant").unwrap();
        let (problem, _) = probe(&paths, "g1", ProviderId::Gemini, &config, &home).await;
        assert_eq!(problem.as_deref(), Some("AUTH_FAILED"));
        gemini::auth_failure::clear(&paths, "g1");
        assert_eq!(
            probe(&paths, "g1", ProviderId::Gemini, &config, &home)
                .await
                .0,
            None
        );
    }
}
