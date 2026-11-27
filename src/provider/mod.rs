//! Provider boundary. Everything that differs between coding-agent providers is
//! dispatched from here; callers never compare provider strings themselves.
//!
//! Account names are unique per provider: `work` can be enrolled once per provider
//! (`codex:work` and `claude:work` coexist). Public identifiers are always
//! provider-qualified (`<provider>:<name>`). A bare name is accepted only where
//! a human addresses an existing account and only while it is unambiguous
//! (`ACCOUNT_AMBIGUOUS` otherwise); a qualified identifier must match the enrolled provider.
pub mod activity;
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod compact;
pub mod conversation;
pub mod dispatch;
pub mod events;
pub mod gemini;
pub mod hooks;
pub mod native;
pub mod ops;
pub mod send;
pub mod session_meters;
pub mod stop_shape;
pub mod usage_cycle;
pub mod usage_meter;

use crate::{
    model::Account,
    paths::{Paths, validate_name},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf, str::FromStr};

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Codex,
    Claude,
    Gemini,
    Antigravity,
}
impl ProviderId {
    pub const ALL: [ProviderId; 4] = [
        ProviderId::Codex,
        ProviderId::Claude,
        ProviderId::Gemini,
        ProviderId::Antigravity,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Codex => "codex",
            ProviderId::Claude => "claude",
            ProviderId::Gemini => "gemini",
            ProviderId::Antigravity => "antigravity",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(ProviderId::Codex),
            "claude" => Ok(ProviderId::Claude),
            "gemini" => Ok(ProviderId::Gemini),
            "antigravity" => Ok(ProviderId::Antigravity),
            other => bail!(
                "UNKNOWN_PROVIDER: '{}' is not a provider; use codex, claude, gemini or antigravity",
                other.chars().take(32).collect::<String>()
            ),
        }
    }
}
impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for ProviderId {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// A provider-qualified account identifier (`codex:work`, `claude:personal`, `gemini:me`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRef {
    pub provider: ProviderId,
    pub name: String,
}
impl AccountRef {
    /// Parse a qualified identifier. Bare names are rejected: no silent default.
    pub fn parse(id: &str) -> Result<Self> {
        let Some((provider, name)) = id.split_once(':') else {
            bail!(
                "ACCOUNT_PROVIDER_REQUIRED: use <provider>:<name>, for example codex:{id}, claude:{id} or gemini:{id}"
            );
        };
        let provider = ProviderId::parse(provider)?;
        validate_name(name)?;
        Ok(Self {
            provider,
            name: name.into(),
        })
    }
    /// Parse a human reference: either qualified or a bare (globally unique) name.
    pub fn parse_lookup(id: &str) -> Result<(Option<ProviderId>, String)> {
        if id.contains(':') {
            let r = Self::parse(id)?;
            Ok((Some(r.provider), r.name))
        } else {
            validate_name(id)?;
            Ok((None, id.into()))
        }
    }
    pub fn id(&self) -> String {
        format!("{}:{}", self.provider, self.name)
    }
    /// Accept only the given provider for an operation it alone implements.
    pub fn require(&self, provider: ProviderId) -> Result<&str> {
        if self.provider != provider {
            bail!(
                "UNSUPPORTED_PROVIDER_OPERATION: {} accounts do not support this operation",
                self.provider
            );
        }
        Ok(&self.name)
    }
    /// Accept only the supervised provider (Codex), with [`Self::require`]'s refusal.
    pub fn require_supervised(&self) -> Result<&str> {
        if !ops::facts(self.provider).supervised {
            bail!(
                "UNSUPPORTED_PROVIDER_OPERATION: {} accounts do not support this operation",
                self.provider
            );
        }
        Ok(&self.name)
    }
}

/// Resolve a (possibly bare) reference against enrolled `(name, provider)` pairs.
/// Names are unique per provider only: a bare name enrolled under several
/// providers is ambiguous and must be qualified.
pub fn resolve<'a>(
    enrolled: impl IntoIterator<Item = (&'a str, ProviderId)>,
    id: &str,
) -> Result<AccountRef> {
    let (wanted, name) = AccountRef::parse_lookup(id)?;
    let providers: Vec<ProviderId> = enrolled
        .into_iter()
        .filter(|(n, _)| *n == name)
        .map(|(_, p)| p)
        .collect();
    match (wanted, providers.as_slice()) {
        (_, []) => bail!("ACCOUNT_NOT_FOUND: no enrolled account named {name}"),
        (Some(wanted), have) => {
            if !have.contains(&wanted) {
                bail!(
                    "ACCOUNT_PROVIDER_MISMATCH: {name} is enrolled as {}:{name}, not {wanted}:{name}",
                    have[0]
                );
            }
            Ok(AccountRef {
                provider: wanted,
                name,
            })
        }
        (None, [provider]) => Ok(AccountRef {
            provider: *provider,
            name,
        }),
        (None, many) => bail!(
            "ACCOUNT_AMBIGUOUS: '{name}' exists as {}; use <provider>:<name>",
            many.iter()
                .map(|p| format!("{p}:{name}"))
                .collect::<Vec<_>>()
                .join(" and ")
        ),
    }
}

/// Names are unique per provider; report the existing owner precisely.
pub fn collision(existing: &Account) -> anyhow::Error {
    anyhow::anyhow!(
        "ACCOUNT_NAME_EXISTS: {}:{} already exists; names are unique per provider (use account login to reauthenticate)",
        existing.provider,
        existing.name
    )
}

/// Where the provider's isolated configuration/credentials live.
pub fn account_home(paths: &Paths, account: &Account) -> Result<PathBuf> {
    match account.provider {
        ProviderId::Codex => codex::home(paths, &account.name),
        ProviderId::Claude => claude::home(paths, &account.name, account.config_home.as_deref()),
        ProviderId::Gemini => gemini::home(paths, &account.name),
        ProviderId::Antigravity => antigravity::home(paths, &account.name),
    }
}

/// Contract 1.62.0 (SM7): the environment variable every provider process that
/// sessionsation starts for a session carries, set to that session's id (the
/// catalog id `api state` reports, not the provider's conversation id).
pub const SESSION_ENV: &str = "SESSIONSATION_SESSION_ID";
/// Name the session a provider child runs for. The builders scrub an inherited
/// value first ([`scrub_session_env`]), so a child never carries its caller's id.
pub fn set_session_env(command: &mut std::process::Command, session_id: &str) {
    command.env(SESSION_ENV, session_id);
}
/// Remove an inherited session id: a supervisor or helper started from inside a
/// session must not hand that session's id to unrelated children.
pub fn scrub_session_env(command: &mut std::process::Command) {
    command.env_remove(SESSION_ENV);
}
/// Contract 1.72.1: the private temp directory of an account's sessions (`session_tmp_root`).
pub fn session_tmp_env(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
) -> Result<Vec<(&'static str, PathBuf)>> {
    let mut dir = match crate::store::load_config(paths)?
        .session_tmp_root
        .as_deref()
    {
        Some("" | "none") => return Ok(vec![]),
        Some(root) => PathBuf::from(root),
        None => paths.root.join("tmp"),
    };
    validate_name(account)?;
    for part in ["", provider.as_str(), account] {
        dir.push(part);
        crate::paths::private_dir(&dir)
            .with_context(|| format!("SESSION_TMP_UNSAFE: {}", dir.display()))?;
    }
    let claude = (provider == ProviderId::Claude).then_some("CLAUDE_CODE_TMPDIR");
    Ok(["TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .chain(claude)
        .map(|n| (n, dir.clone()))
        .collect())
}
pub fn session_tmp_of(
    p: &Paths,
    s: &crate::catalog::Session,
) -> Result<Vec<(&'static str, PathBuf)>> {
    session_tmp_env(p, ProviderId::parse(&s.provider)?, &s.account)
}
pub fn session_tmp_capability() -> serde_json::Value {
    let since = "1.72.1";
    serde_json::json!({"since_contract":since,"env":["TMPDIR","TMP","TEMP","CLAUDE_CODE_TMPDIR"],"default_root":"<data_root>/tmp/<provider>/<account>","configurable_by":"session_tmp_root","codex_app_server":true,"applies_at":"next app-server start or restart","covers":["launch","codex_pane","codex_app_server","codex_stdio_app_server","headless","hosted","attach"]})
}
/// `NAME=value` for the `env` prefix of a Codex pane command.
pub fn session_env_assignment(session_id: &str) -> String {
    format!("{SESSION_ENV}={}", crate::terminal::shell_quote(session_id))
}

#[derive(Clone, Debug, Serialize)]
pub struct Capability {
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}
pub const fn yes() -> Capability {
    Capability {
        supported: true,
        reason: None,
    }
}
pub const fn no(reason: &'static str) -> Capability {
    Capability {
        supported: false,
        reason: Some(reason),
    }
}

/// Why a provider has no shared app-server to restart.
pub const NO_SERVER: &str = "only Codex accounts run a shared app-server";

/// Explicit per-operation support. Never infer support from a provider name.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderCapabilities {
    pub enrollment: Capability,
    pub adopt_config_dir: Capability,
    pub login: Capability,
    pub auth_status: Capability,
    pub usage: Capability,
    pub new: Capability,
    pub resume: Capability,
    pub rename: Capability,
    pub native_rename: Capability,
    pub stop: Capability,
    pub restart: Capability,
    pub account_switch: Capability,
    pub relocate: Capability,
    pub move_space: Capability,
    pub archive: Capability,
    pub delete_session: Capability,
    pub submit: Capability,
    pub queue: Capability,
    pub steer: Capability,
    pub account_removal: Capability,
    pub account_deletion: Capability,
    pub account_rename: Capability,
    /// Official sign-out of the stored login (reason names the mechanism).
    pub account_logout: Capability,
    pub account_restart_server: Capability,
    /// Bounded headless execution through `api send` (Claude `claude -p`).
    pub headless_delivery: Capability,
    pub account_limits: Capability,
    pub cross_provider_conversion: Capability,
}

pub fn capabilities(provider: ProviderId) -> ProviderCapabilities {
    match provider {
        ProviderId::Codex => codex::capabilities(),
        ProviderId::Claude => claude::capabilities(),
        ProviderId::Gemini => gemini::capabilities(),
        ProviderId::Antigravity => antigravity::capabilities(),
    }
}

#[cfg(test)]
mod tests;
