//! Redeem a stored Codex rate-limit reset credit (contract 1.53.0).
//!
//! `api account reset-credit codex:NAME [--credit-id ID] [--request-id UUID]` asks
//! the account's app-server (`account/rateLimitResetCredit/consume`) to spend one
//! earned reset credit. Spending is not reversible, so every attempt is journaled in
//! `operations/account-reset-credits/<request_id>.json` before the request is sent,
//! and the request id IS the backend idempotency key: a retry with the same
//! `--request-id` sends the same key (the backend answers `alreadyRedeemed` when that
//! attempt already succeeded). Nothing here ever retries with a new key.
//!
//! ```text
//! requesting -> complete (outcome)
//!            \-> failed     (the app-server answered with an error; nothing to replay)
//!            \-> uncertain  (no answer; retry with the same request id)
//! ```
use crate::{
    model::{ResetCredit, ResetCredits},
    paths::{Paths, atomic_write, private_dir},
    provider::ProviderId,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

pub const METHOD: &str = "account/rateLimitResetCredit/consume";
/// Every `ConsumeAccountRateLimitResetCreditOutcome` of Codex app-server 0.165.0.
pub const OUTCOMES: &[&str] = &["reset", "nothingToReset", "noCredit", "alreadyRedeemed"];
pub const KIND: &str = "account_reset_credit";
pub mod claude;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub schema_version: u32,
    /// The request id, also sent as the backend `idempotencyKey`.
    pub id: String,
    pub kind: String,
    pub provider: ProviderId,
    pub account_id: String,
    pub credit_id: Option<String>,
    pub idempotency_key: String,
    pub phase: String,
    pub outcome: Option<String>,
    /// Requests sent with this key (1 + retries with the same request id).
    pub attempts: u32,
    pub detail: String,
    /// The provider build or API that answered (the field name predates Claude).
    pub codex_version: String,
    /// Claude: the backend `result` and `reason` (`reset_rate_limits`), as sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_result: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::ACCOUNT_RESET_CREDITS)
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    validate_request_id(id)?;
    Ok(dir(paths).join(format!("{id}.json")))
}
pub fn validate_request_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .ok()
        .filter(|u| u.hyphenated().to_string() == id)
        .context("INVALID_REQUEST_ID: --request-id must be a lowercase hyphenated UUID")?;
    Ok(())
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = crate::daemon::now();
    private_dir(&dir(paths))?;
    atomic_write(&file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|f| f.is_file())
}
pub fn read(paths: &Paths, id: &str) -> Result<Record> {
    let bytes = fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?;
    serde_json::from_slice(&bytes).context("UNREADABLE_OPERATION_RECORD")
}
pub fn list(paths: &Paths) -> Result<Vec<Record>> {
    let mut all = vec![];
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return Ok(all);
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if let Ok(r) = read(paths, id) {
            all.push(r);
        }
    }
    all.sort_by_key(|r| (r.created_at, r.id.clone()));
    Ok(all)
}
/// Operation row for `api operations`.
pub fn public(r: &Record) -> Value {
    json!({"id":r.id,"kind":KIND,"phase":r.phase,"detail":r.detail,
    "account_id":r.account_id,"credit_id":r.credit_id,"request_id":r.id,
    "outcome":r.outcome,"attempts":r.attempts,"provider":r.provider,"provider_result":r.provider_result,
    "uncertain":r.phase == "uncertain","terminal":matches!(r.phase.as_str(),"complete"|"failed"),
    "created_at":r.created_at,"updated_at":r.updated_at,
    "replays":false,
    "next":match r.phase.as_str() {
        "complete" | "failed" => "none",
        _ => "retry api account reset-credit with the same --request-id (same idempotency key)",
    }})
}

/// Journal one attempt BEFORE its request is sent: a new record, or the existing
/// record of this request id (same account and credit) with one more attempt.
pub fn admit(
    paths: &Paths,
    provider: ProviderId,
    account_id: &str,
    credit_id: Option<&str>,
    request_id: &str,
    codex_version: &str,
) -> Result<Record> {
    validate_request_id(request_id)?;
    let mut record = if exists(paths, request_id) {
        let r = read(paths, request_id)?;
        ensure!(
            r.account_id == account_id && r.credit_id.as_deref() == credit_id,
            "REQUEST_ID_CONFLICT: request {request_id} was used for {} (credit {}); use a new request id for a different attempt",
            r.account_id,
            r.credit_id.as_deref().unwrap_or("any")
        );
        Record {
            phase: "requesting".into(),
            attempts: r.attempts + 1,
            detail: "retrying with the same idempotency key".into(),
            codex_version: codex_version.into(),
            ..r
        }
    } else {
        let now = crate::daemon::now();
        Record {
            schema_version: 1,
            id: request_id.into(),
            kind: KIND.into(),
            provider,
            account_id: account_id.into(),
            credit_id: credit_id.map(str::to_owned),
            idempotency_key: request_id.into(),
            phase: "requesting".into(),
            outcome: None,
            attempts: 1,
            detail: if crate::provider::ops::facts(provider).reset_credits
                == crate::provider::ops::Reader::OAuth
            {
                "claim sent to the Claude API (reset_rate_limits)".into()
            } else {
                "request sent to the account app-server".into()
            },
            codex_version: codex_version.into(),
            provider_result: None,
            created_at: now,
            updated_at: now,
        }
    };
    save(paths, &mut record)?;
    Ok(record)
}

pub fn finish(
    paths: &Paths,
    record: &mut Record,
    phase: &str,
    outcome: Option<&str>,
    detail: &str,
) -> Result<()> {
    record.phase = phase.into();
    if outcome.is_some() {
        record.outcome = outcome.map(str::to_owned);
    }
    record.detail = detail.into();
    save(paths, record)
}

/// The `outcome` of a consume response; anything else is not trusted.
pub fn parse_outcome(result: &Value) -> Result<String> {
    match result["outcome"].as_str() {
        Some(o) if OUTCOMES.contains(&o) => Ok(o.into()),
        _ => bail!("unexpected consume result (no known outcome)"),
    }
}

/// `rateLimitResetCredits` of an `account/rateLimits/read` response.
pub fn parse_summary(summary: &Value, at: i64) -> Option<ResetCredits> {
    let available = summary["availableCount"].as_i64()?.max(0) as u64;
    let credits = summary["credits"].as_array().map(|rows| {
        rows.iter()
            .filter_map(|c| {
                Some(ResetCredit {
                    id: c["id"].as_str()?.to_owned(),
                    status: c["status"].as_str().map(str::to_owned),
                    expires_at: c["expiresAt"].as_i64(),
                    ..Default::default()
                })
            })
            .collect()
    });
    Some(ResetCredits {
        available,
        observed_at: at,
        credits,
        ..Default::default()
    })
}

/// A `usage.reset_credits` value with `fresh` and `age_seconds` (contract 1.65.0), like the
/// meter: fresh while the read is younger than `stale_seconds`. Null stays null.
pub fn with_age(mut credits: Value, now: i64, stale_seconds: i64) -> Value {
    if let Some(at) = credits["observed_at"].as_i64() {
        credits["age_seconds"] = json!((now - at).max(0));
        credits["fresh"] = json!(at > 0 && now >= at && now - at < stale_seconds);
    }
    credits
}

/// The error for a consume request that did not produce an outcome. The record is
/// already finished; the message names the codex version and how to retry.
pub fn failure(error: &anyhow::Error, version: &str, request_id: &str) -> (&'static str, String) {
    if crate::rpc::unknown_method(error) {
        (
            "failed",
            crate::provider::ops::unsupported(format_args!(
                "codex {version} does not support {METHOD} (method not found); update Codex (0.165.0 has it). No credit was spent"
            ))
            .to_string(),
        )
    } else if crate::rpc::rejected(error) {
        (
            "failed",
            format!(
                "RESET_CREDIT_FAILED: codex {version} rejected {METHOD}: {error}; not retried. Retry this same attempt with --request-id {request_id}"
            ),
        )
    } else {
        (
            "uncertain",
            format!(
                "RESET_CREDIT_OUTCOME_UNKNOWN: {METHOD} on codex {version} got no answer ({error}); not retried. Retry with --request-id {request_id}: the same idempotency key cannot spend a second credit"
            ),
        )
    }
}

/// Capability `account_reset_credit` (contract 1.53.0; Claude since 1.60.0).
pub fn capability() -> Value {
    json!({"since_contract":"1.53.0","providers":["codex","claude"],"method":METHOD,
        "command":"api account reset-credit PROVIDER:NAME [--credit-id ID] [--request-id UUID] [--kind 5-hour|weekly|fable|full]",
        "claude":{"since_contract":"1.60.0","status":{"method":"GET","url":format!("{}{}",crate::provider::claude::oauth::API_BASE,claude::STATUS_PATH),"block":"cedar_ember","cache_seconds":claude::CACHE_SECONDS,"polled":true,
                // Contract 1.65.0: refreshed with the meter (every meter read is this request).
                "refreshed_with_meter_seconds":crate::provider::claude::meter::REFRESH_SECONDS},
            "claim":{"method":"POST","path":"/api/organizations/<organizationUuid>/reset_rate_limits","body":["program","grant_id","request_id"],"program":claude::PROGRAM,"request_id":"request_id"},
            "outcomes":claude::OUTCOMES,"kinds":["5-hour","weekly","fable","full"],"credit_id":"grant id ^[a-z0-9_-]{1,40}$",
            "grant_selection":"--credit-id, else a usable grant of --kind, else next_grant_id, else the first usable grant","no_usable_grant":{"outcome":"noCredit","sent":false},
            // guard-allow(oauth): capability text names the credential source; nothing is read here
            "credentials":{"reads":"CLAUDE_CONFIG_DIR/.credentials.json claudeAiOauth","token_in":"curl stdin only","renewal":"only when expiring within 300 s or after a 401, under Claude Code's refresh lock",
                "refresh_lock":crate::provider::claude::oauth::REFRESH_LOCK,"storage_lock":crate::provider::claude::oauth::STORAGE_LOCK,"write":"atomic, mode 0600, other fields preserved, compare-and-swap on refreshToken"},
            "errors":["AUTH_STALE","AUTH_REQUIRED","RESET_CREDITS_READ_FAILED","RESET_CREDITS_UNREADABLE"]},
        "idempotent_by":"request_id","idempotency_key":"request_id",
        "request_id_generated_when_omitted":true,"journaled_before_request":true,
        "journal":crate::layout::ACCOUNT_RESET_CREDITS,"auto_retry":false,"retry":"same_request_id_only",
        "outcomes":OUTCOMES,"result_fields":["account_id","request_id","outcome","rate_limits_after","attempts","operation"],"claude_result_fields":["credit_id","claude","rate_limits_after_error","sent","cycles_started"],
        "rate_limits_refreshed_after":["reset","alreadyRedeemed"],
        "lock":"account_operation_lock","reversible":false,
        "usage_field":{"field":"usage.reset_credits","shape":{"available":"integer","observed_at":"unix_seconds","credits":"array|null","fresh":"boolean","age_seconds":"integer"},"absent":null},
        "errors":[crate::provider::ops::UNSUPPORTED,"INVALID_REQUEST_ID","REQUEST_ID_CONFLICT","ACCOUNT_BUSY","AUTH_REQUIRED","AUTH_STALE","RESET_CREDIT_FAILED","RESET_CREDIT_OUTCOME_UNKNOWN"]})
}

#[cfg(test)]
mod tests;
