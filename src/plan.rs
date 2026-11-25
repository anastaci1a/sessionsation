//! Read-only, non-secret subscription plan of an account (contract 1.46.7).
//!
//! Only values the provider already reported through an existing parsed output are
//! kept: Claude `auth status` (`subscriptionType`, `rateLimitTier` when present) and
//! the Codex usage snapshot (`planType`). Nothing here reads a credential file.
use crate::provider::ProviderId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanObservation {
    /// Provider value exactly as reported (never normalised).
    pub raw: String,
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    /// Unix seconds of the observation the value came from.
    pub verified_at: i64,
}

/// Human label for a raw value, only where the meaning is certain.
pub fn name(provider: ProviderId, raw: &str) -> Option<&'static str> {
    let raw = raw.to_ascii_lowercase();
    crate::provider::ops::facts(provider)
        .plan_names
        .iter()
        .find(|(key, _)| *key == raw)
        .map(|(_, label)| *label)
}

/// The public `plan` object; null when nothing was observed.
pub fn project(provider: ProviderId, observation: Option<&PlanObservation>, source: &str) -> Value {
    match observation {
        Some(o) if !o.raw.is_empty() => json!({
            "provider": provider, "raw": o.raw, "name": name(provider, &o.raw),
            "rate_limit_tier": o.rate_limit_tier, "source": source, "verified_at": o.verified_at,
        }),
        _ => Value::Null,
    }
}

/// Claude observation from a successful `auth status` summary (`probe` output).
pub fn from_claude_auth(mode: &Value, at: i64) -> Option<PlanObservation> {
    let raw = mode["subscription_type"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    Some(PlanObservation {
        raw: raw.to_owned(),
        rate_limit_tier: mode["rate_limit_tier"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        verified_at: at,
    })
}

/// Codex observation from the last usage snapshot.
pub fn from_codex_usage(usage: &crate::model::Usage, now: i64) -> Option<PlanObservation> {
    let raw = usage.plan_type.as_deref().filter(|s| !s.is_empty())?;
    (usage.observed_at > 0).then(|| PlanObservation {
        raw: raw.to_owned(),
        rate_limit_tier: None,
        verified_at: usage.observed_at.min(now),
    })
}

#[cfg(test)]
mod tests;
