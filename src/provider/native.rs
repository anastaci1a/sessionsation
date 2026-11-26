//! Dispatch for providers whose sessions are native files inside an account
//! home (Claude Code, Gemini CLI and Antigravity CLI). They share the
//! observation, catalog and delivery model; everything that differs is chosen
//! here by `ProviderId`, so callers never compare provider strings.
use super::{
    ProviderId, antigravity,
    claude::{self, Enrolled, Live, Observation, Transcript},
    gemini,
};
use crate::{
    catalog,
    config::Config,
    paths::{Paths, private_dir},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
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
pub fn default_home(paths: &Paths, provider: ProviderId, name: &str) -> Result<PathBuf> {
    match provider {
        ProviderId::Gemini => gemini::home(paths, name),
        ProviderId::Antigravity => antigravity::home(paths, name),
        ProviderId::Claude | ProviderId::Codex => claude::default_home(paths, name),
    }
}

/// Accounts of one native provider from a read-only registry snapshot (daemon need
/// not run); `home` resolves each account's home.
pub(crate) fn enrolled_in(
    paths: &Paths,
    provider: ProviderId,
    home: impl Fn(&crate::model::Account) -> Result<PathBuf>,
) -> Result<Vec<Enrolled>> {
    if !paths.root.join(crate::layout::REGISTRY_DB).exists() {
        return Ok(vec![]);
    }
    crate::store::load_state_snapshot(paths)?
        .accounts
        .values()
        .filter(|a| a.provider == provider)
        .map(|a| {
            Ok(Enrolled {
                name: a.name.clone(),
                home: home(a)?,
                enabled: a.enabled,
                auth_required: a.auth_required,
            })
        })
        .collect()
}
/// Codex accounts are answered by Claude's tables.
fn table_provider(provider: ProviderId) -> ProviderId {
    if provider == ProviderId::Codex {
        ProviderId::Claude
    } else {
        provider
    }
}
pub fn account(paths: &Paths, provider: ProviderId, name: &str) -> Result<Enrolled> {
    let provider = table_provider(provider);
    let enrolled = match provider {
        ProviderId::Gemini => gemini::enrolled(paths),
        ProviderId::Antigravity => antigravity::enrolled(paths),
        _ => claude::enrolled(paths),
    }?;
    if let Some(a) = enrolled.into_iter().find(|a| a.name == name) {
        return Ok(a);
    }
    ensure!(
        !deleted(paths, provider, name)?,
        "ACCOUNT_DELETED: {provider}:{name} was deleted locally; its conversation history is preserved but it cannot be resumed through this account"
    );
    let (initial, rest) = provider.as_str().split_at(1);
    bail!(
        "ACCOUNT_NOT_FOUND: no enrolled {}{rest} account with this name",
        initial.to_uppercase()
    )
}
/// True when a completed local deletion removed this (no longer enrolled) account.
pub fn deleted(paths: &Paths, provider: ProviderId, name: &str) -> Result<bool> {
    let id = format!("{}:{name}", table_provider(provider));
    Ok(crate::account_deletion::list(paths)?
        .iter()
        .any(|r| r.account_id == id && r.phase == "complete"))
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
            Ok(s) => {
                let mut mode = serde_json::json!({"logged_in":s.logged_in,"auth_method":s.auth_method,"api_provider":s.api_provider,"subscription_type":s.subscription_type});
                if let Some(tier) = &s.rate_limit_tier {
                    mode["rate_limit_tier"] = serde_json::json!(tier);
                }
                (s.problem().map(str::to_owned), mode)
            }
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
/// Contract 1.66.2: the session has no transcript (it was stopped before its first turn) and
/// its provider can start it again under the same conversation id.
pub fn without_history(session: &catalog::Session, observation: &Observation) -> bool {
    crate::provider::ops::holds(&session.provider, |f| f.restart_without_history)
        && observation.availability == "stale"
        && observation.transcript.is_none()
}
/// The preconditions of starting a session without history again (see [`without_history`]).
pub fn ensure_fresh_start(session: &catalog::Session, observation: &Observation) -> Result<()> {
    claude::launch::ensure_fresh_start(session, observation)
}
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

/// Create a private (0700) home under its already private parent, or accept an existing
/// one that is a 0700 directory owned by this user and not a symlink (never wiped).
/// Returns (home, existed).
pub(crate) fn create_private_home(home: PathBuf) -> Result<(PathBuf, bool)> {
    use std::os::unix::fs::MetadataExt;
    private_dir(home.parent().context("missing parent")?)?;
    match fs::symlink_metadata(&home) {
        Ok(m) => {
            ensure!(
                m.file_type().is_dir()
                    && m.uid() == unsafe { libc::geteuid() }
                    && m.mode() & 0o777 == 0o700,
                "UNSAFE_ACCOUNT_HOME: existing {} must be a 0700 directory owned by this user and not a symlink; it was left unchanged",
                home.display()
            );
            Ok((home, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            private_dir(&home)?;
            Ok((home, false))
        }
        Err(e) => Err(e.into()),
    }
}
/// The observation of a Gemini or Antigravity session from its transcript facts
/// (`found`: path, directory, start, bytes) and the live process probe;
/// `detect_switch` reports a recorded process that now holds another conversation.
pub(crate) fn native_observation(
    recorded: Option<&catalog::Attachment>,
    session_id: &str,
    found: Option<(PathBuf, Option<String>, Option<String>, u64)>,
    live: &std::result::Result<Vec<Live>, String>,
    detect_switch: bool,
) -> Observation {
    let (transcript, directory, started_at, bytes) = match found {
        Some((p, cwd, started, bytes)) => (Some(p), cwd, started, Some(bytes)),
        None => (None, None, None, None),
    };
    let mut o = Observation {
        availability: "unknown",
        native_status: None,
        native_name: None,
        pid: None,
        process_start: None,
        directory,
        transcript,
        transcript_bytes: bytes,
        started_at,
        attachment: None,
        detail: None,
        native_kind: None,
        job_id: None,
        attachment_switched: false,
        forked_from: None,
        active_since: None,
    };
    match live {
        Err(e) => o.detail = Some(format!("PROBE_FAILED: {e}")),
        Ok(live) => match live.iter().find(|l| l.session_id == session_id) {
            Some(l) => {
                o.availability = "running";
                o.attachment = claude::host_attachment(l, recorded);
                o.pid = Some(l.pid);
                o.process_start = Some(l.process_start);
                o.active_since = claude::process_started_at(l.process_start);
                if l.cwd.is_some() {
                    o.directory = l.cwd.clone();
                }
            }
            None => {
                // The recorded process is alive but now holds another conversation.
                o.attachment_switched = detect_switch
                    && recorded.is_some_and(|r| {
                        live.iter().any(|l| {
                            l.pid == r.pid
                                && l.process_start == r.process_start
                                && l.session_id != session_id
                        })
                    });
                o.availability = if o.transcript.is_some() {
                    "stopped"
                } else {
                    "stale"
                };
            }
        },
    }
    o
}
/// The non-empty arguments of a process (its command line, at most 64 KiB).
pub(crate) fn arguments(pid: u32) -> Option<Vec<String>> {
    let mut bytes = vec![];
    fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(
        bytes
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
}
impl Live {
    /// A verified native process of Gemini or Antigravity: only its identity and cwd.
    pub(crate) fn native(pid: u32, process_start: u64, session_id: String) -> Self {
        Live {
            pid,
            process_start,
            session_id,
            cwd: fs::read_link(format!("/proc/{pid}/cwd"))
                .ok()
                .and_then(|p| p.to_str().map(str::to_owned)),
            status: None,
            name: None,
            kind: None,
            job_id: None,
            registry_started_ms: None,
            spare: false,
            parked_job_id: None,
            name_source: None,
        }
    }
}

#[cfg(test)]
mod tests;
