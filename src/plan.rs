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
    match (provider, raw.to_ascii_lowercase().as_str()) {
        (ProviderId::Claude, "pro") => Some("Claude Pro"),
        (ProviderId::Codex, "free") => Some("ChatGPT Free"),
        (ProviderId::Codex, "plus") => Some("ChatGPT Plus"),
        (ProviderId::Codex, "pro") => Some("ChatGPT Pro"),
        (ProviderId::Codex, "team") => Some("ChatGPT Team"),
        (ProviderId::Codex, "business") => Some("ChatGPT Business"),
        (ProviderId::Codex, "enterprise") => Some("ChatGPT Enterprise"),
        _ => None,
    }
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
mod tests {
    use super::*;
    #[test]
    fn names_only_known_values_and_keeps_raw() {
        assert_eq!(name(ProviderId::Claude, "pro"), Some("Claude Pro"));
        // `auth status` reports "max" for both Max and Ultra: no name, raw kept.
        assert_eq!(name(ProviderId::Claude, "max"), None);
        assert_eq!(name(ProviderId::Codex, "prolite"), None);
        let o = PlanObservation {
            raw: "prolite".into(),
            rate_limit_tier: None,
            verified_at: 5,
        };
        let p = project(ProviderId::Codex, Some(&o), "usage_snapshot");
        assert_eq!(p["raw"], "prolite");
        assert!(p["name"].is_null() && p["rate_limit_tier"].is_null());
        assert_eq!(p["verified_at"], 5);
        assert!(project(ProviderId::Gemini, None, "x").is_null());
    }
    #[test]
    fn claude_auth_carries_tier_only_when_reported() {
        let m = json!({"subscription_type":"max"});
        let o = from_claude_auth(&m, 9).unwrap();
        assert_eq!(
            (o.raw.as_str(), o.rate_limit_tier.clone(), o.verified_at),
            ("max", None, 9)
        );
        let m = json!({"subscription_type":"max","rate_limit_tier":"default_claude_max_5x"});
        assert_eq!(
            from_claude_auth(&m, 9).unwrap().rate_limit_tier.as_deref(),
            Some("default_claude_max_5x")
        );
        assert!(from_claude_auth(&json!({"subscription_type":null}), 9).is_none());
    }
    #[test]
    fn codex_plan_uses_the_usage_observation_time() {
        let u = crate::model::Usage {
            plan_type: Some("plus".into()),
            observed_at: 100,
            ..Default::default()
        };
        assert_eq!(from_codex_usage(&u, 500).unwrap().verified_at, 100);
        assert_eq!(from_codex_usage(&u, 50).unwrap().verified_at, 50);
        assert!(from_codex_usage(&crate::model::Usage::default(), 5).is_none());
    }
}
