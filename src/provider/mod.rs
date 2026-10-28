//! Provider boundary. Everything that differs between coding-agent providers is
//! dispatched from here; callers never compare provider strings themselves.
//!
//! Account names are globally unique across providers: `work` can be enrolled
//! as `codex:work` or `claude:work`, never both. Public identifiers are always
//! provider-qualified (`<provider>:<name>`). A bare name is accepted only where
//! a human addresses an existing account (it is unambiguous because names are
//! global); a qualified identifier must match the enrolled provider.
pub mod claude;
pub mod codex;

use crate::{
    model::Account,
    paths::{Paths, validate_name},
};
use anyhow::{Result, bail};
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
}
impl ProviderId {
    pub const ALL: [ProviderId; 2] = [ProviderId::Codex, ProviderId::Claude];
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Codex => "codex",
            ProviderId::Claude => "claude",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(ProviderId::Codex),
            "claude" => Ok(ProviderId::Claude),
            other => bail!(
                "UNKNOWN_PROVIDER: '{}' is not a provider; use codex or claude",
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

/// A provider-qualified account identifier (`codex:work`, `claude:personal`).
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
                "ACCOUNT_PROVIDER_REQUIRED: use <provider>:<name>, for example codex:{id} or claude:{id}"
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
}

/// Resolve a (possibly bare) reference against enrolled `(name, provider)` pairs.
pub fn resolve<'a>(
    enrolled: impl IntoIterator<Item = (&'a str, ProviderId)>,
    id: &str,
) -> Result<AccountRef> {
    let (wanted, name) = AccountRef::parse_lookup(id)?;
    let Some((_, provider)) = enrolled.into_iter().find(|(n, _)| *n == name) else {
        bail!("ACCOUNT_NOT_FOUND: no enrolled account named {name}");
    };
    if let Some(wanted) = wanted
        && wanted != provider
    {
        bail!(
            "ACCOUNT_PROVIDER_MISMATCH: {name} is enrolled as {provider}:{name}, not {wanted}:{name}"
        );
    }
    Ok(AccountRef { provider, name })
}

/// Names are global across providers; report the existing owner precisely.
pub fn collision(existing: &Account) -> anyhow::Error {
    anyhow::anyhow!(
        "ACCOUNT_NAME_EXISTS: account name '{}' already exists as {}:{}; names are unique across providers (use account login to reauthenticate)",
        existing.name,
        existing.provider,
        existing.name
    )
}

/// Where the provider's isolated configuration/credentials live.
pub fn account_home(paths: &Paths, account: &Account) -> Result<PathBuf> {
    match account.provider {
        ProviderId::Codex => codex::home(paths, &account.name),
        ProviderId::Claude => claude::home(paths, &account.name, account.config_home.as_deref()),
    }
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
    /// Bounded headless execution through `api send` (Claude `claude -p`).
    pub headless_delivery: Capability,
    pub account_limits: Capability,
    pub cross_provider_conversion: Capability,
}

pub fn capabilities(provider: ProviderId) -> ProviderCapabilities {
    match provider {
        ProviderId::Codex => codex::capabilities(),
        ProviderId::Claude => claude::capabilities(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_are_provider_qualified_and_mismatches_are_explicit() {
        assert_eq!(
            AccountRef::parse("claude:work").unwrap(),
            AccountRef {
                provider: ProviderId::Claude,
                name: "work".into()
            }
        );
        let bare = AccountRef::parse("work").unwrap_err().to_string();
        assert!(bare.contains("ACCOUNT_PROVIDER_REQUIRED"), "{bare}");
        assert!(
            AccountRef::parse("gemini:work")
                .unwrap_err()
                .to_string()
                .contains("UNKNOWN_PROVIDER")
        );
        assert!(AccountRef::parse("claude:../x").is_err());
        let enrolled = [("work", ProviderId::Codex), ("home", ProviderId::Claude)];
        assert_eq!(
            resolve(enrolled, "work").unwrap().provider,
            ProviderId::Codex
        );
        assert_eq!(
            resolve(enrolled, "claude:home").unwrap().provider,
            ProviderId::Claude
        );
        let mismatch = resolve(enrolled, "claude:work").unwrap_err().to_string();
        assert!(mismatch.contains("ACCOUNT_PROVIDER_MISMATCH"), "{mismatch}");
        assert!(mismatch.contains("codex:work"), "{mismatch}");
        assert!(
            resolve(enrolled, "codex:missing")
                .unwrap_err()
                .to_string()
                .contains("ACCOUNT_NOT_FOUND")
        );
        assert!(
            AccountRef::parse("claude:work")
                .unwrap()
                .require(ProviderId::Codex)
                .unwrap_err()
                .to_string()
                .contains("UNSUPPORTED_PROVIDER_OPERATION")
        );
    }
    #[test]
    fn collisions_name_the_existing_owner() {
        let mut a = Account::new("work".into(), String::new());
        a.provider = ProviderId::Claude;
        let e = collision(&a).to_string();
        assert!(e.starts_with("ACCOUNT_NAME_EXISTS"), "{e}");
        assert!(e.contains("claude:work"), "{e}");
    }
}
