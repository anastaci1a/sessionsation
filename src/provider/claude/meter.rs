//! Claude account usage meter without a running session (contract 1.62.0, SM8).
//!
//! Claude Code itself reads `GET /api/oauth/usage` (its `fetchUtilization`, read `plain`) for
//! `/usage`: the answer's `five_hour` and `seven_day` windows are
//! `{utilization: percent 0-100 | null, resets_at: ISO 8601 | null}` (2.1.314's schema `an`;
//! it also lists `seven_day_opus`, `seven_day_sonnet`, `extra_usage`, ... which are not read).
//! The same request, through the 1.60.0 OAuth owner ([`super::oauth`]), keeps an idle
//! account's meter current. Readings go to the statusline recorder's owner
//! ([`super::record_windows`]): the fresher reading wins, but within one reset cycle a
//! window never goes down, whichever source reported it.
//!
//! Since 1.65.0 every read asks for the `cedar_ember` status too
//! ([`crate::reset_credit::claude::STATUS_PATH`]), so `usage.reset_credits` stays as current as
//! the meter, and a grant whose `resets_left` fell is a reset made elsewhere: the windows it
//! clears start a new reset cycle ([`crate::provider::usage_cycle`]).
//!
//! - On demand (`api usage`, `api account limits`): at most one read per account per
//!   [`CACHE_SECONDS`] (a failed attempt counts); the token is renewed only as
//!   [`super::oauth::access_token`] allows (expiring within 5 minutes, or after a 401).
//! - Supervisor ([`monitor`]): every minute, an enabled Claude account whose meter or reset
//!   credits are older than [`REFRESH_SECONDS`] is read with the stored token only; a token
//!   due for renewal is left alone (the next on-demand read or Claude Code renews it).
use super::{
    UsageSource,
    oauth::{self, Client, SendError},
    record_windows,
};
use crate::{
    paths::{Paths, atomic_write, private_dir},
    provider::ProviderId,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Every meter read (contract 1.65.0): Claude Code's usage answer with its `cedar_ember` status,
/// so one request refreshes the windows and the reset credits.
pub const READ_PATH: &str = crate::reset_credit::claude::STATUS_PATH;
/// An on-demand read is reused (not repeated) for this long.
pub const CACHE_SECONDS: i64 = 60;
/// The supervisor reads a meter older than this (seconds); [`REFRESH_ENV`] overrides it and
/// `0` turns the background read off.
pub const REFRESH_SECONDS: i64 = 600;
pub const REFRESH_ENV: &str = "SESSIONSATION_CLAUDE_METER_SECONDS";

/// Whether a read may renew the access token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renew {
    /// As [`oauth::access_token`] allows (caller requests).
    Allowed,
    /// Never: the stored token or nothing (supervisor reads).
    Never,
}

fn attempt_file(home: &Path) -> PathBuf {
    home.join(".sessionsation/usage-read.json")
}
/// The last read attempt: `{attempted_at, ok, code, message}` (never a token).
pub fn last_attempt(home: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(attempt_file(home)).ok()?).ok()
}
fn note_attempt(home: &Path, value: &Value) {
    let dir = home.join(".sessionsation");
    if private_dir(&dir).is_ok() {
        let _ = atomic_write(
            &attempt_file(home),
            &serde_json::to_vec(value).unwrap_or_default(),
        );
    }
}

/// One usage-answer window as a recorder window, or null (no utilization or no reset time:
/// nothing current is known about it).
fn window(v: &Value) -> Value {
    let used = v["utilization"].as_f64().filter(|u| u.is_finite());
    let resets = v["resets_at"]
        .as_str()
        .and_then(crate::reset_credit::claude::parse_time);
    match (used, resets) {
        // Claude Code shows at most 100% (`hve`); an over-limit answer is "at the limit".
        (Some(u), Some(r)) => json!({"used_percentage":u.clamp(0., 100.),"resets_at":r}),
        // Contract 1.65.0: a window the answer names with no reset time and no usage has not
        // started; it starts on first use (Claude's 5-hour window after its reset, while idle).
        (u, None) if v.is_object() && u.is_none_or(|u| u == 0.0) => {
            json!({"starts_on_first_use":true})
        }
        _ => Value::Null,
    }
}
/// `five_hour` / `seven_day` of a usage answer in the recorder's shape. A window the answer does
/// not name is left out; one it names with no current value is null (contract 1.65.0: right
/// after a redeem that clears it, the window is dropped).
pub fn windows(body: &Value) -> Value {
    let mut out = json!({});
    for key in ["five_hour", "seven_day"] {
        if body.get(key).is_some() {
            out[key] = window(&body[key]);
        }
    }
    out
}
/// Record the windows of one usage answer as an OAuth reading. Ok(false): the answer names
/// neither window (nothing observed) or nothing changed.
pub fn record_answer(home: &Path, body: &Value, now: i64) -> Result<bool> {
    ensure!(
        body.is_object(),
        "USAGE_READ_FAILED: the usage answer is not a JSON object"
    );
    if body.get("five_hour").is_none() && body.get("seven_day").is_none() {
        return Ok(false);
    }
    record_windows(home, &windows(body), now, UsageSource::OAuth, None)
}
/// [`record_answer`] under a held [`super::recorder_lock`].
pub fn record_answer_locked(
    lock: &super::RecorderLock,
    home: &Path,
    body: &Value,
    now: i64,
) -> Result<bool> {
    ensure!(
        body.is_object(),
        "USAGE_READ_FAILED: the usage answer is not a JSON object"
    );
    if body.get("five_hour").is_none() && body.get("seven_day").is_none() {
        return Ok(false);
    }
    super::record_windows_locked(lock, home, &windows(body), now, UsageSource::OAuth, None)
}

fn code_of(text: &str) -> &str {
    let code = text.split(':').next().unwrap_or_default();
    if !code.is_empty() && code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        code
    } else {
        "USAGE_READ_FAILED"
    }
}

/// Read the account's usage now (unless `force` is false and an attempt is younger than
/// [`CACHE_SECONDS`]). Never fails: the result says what happened, as
/// `{attempted, ok, code, message, attempted_at}`; the meter keeps its last value on failure.
pub async fn read(client: &Client, home: &Path, account: &str, force: bool, renew: Renew) -> Value {
    let now = crate::daemon::now();
    if !force
        && let Some(last) = last_attempt(home)
        && last["attempted_at"]
            .as_i64()
            .is_some_and(|at| (0..CACHE_SECONDS).contains(&(now - at)))
    {
        let mut last = last;
        last["attempted"] = json!(false);
        return last;
    }
    // Claim the slot first so concurrent callers do not all send.
    note_attempt(
        home,
        &json!({"attempted_at":now,"ok":Value::Null,"code":Value::Null,"message":Value::Null}),
    );
    let outcome = send(client, home, account, renew).await;
    let (ok, code, message) = match outcome {
        Ok(()) => (true, Value::Null, Value::Null),
        Err(e) => {
            let text = format!("{e:#}");
            (false, json!(code_of(&text)), json!(text))
        }
    };
    let result = json!({"attempted_at":now,"ok":ok,"code":code,"message":message});
    note_attempt(home, &result);
    let mut result = result;
    result["attempted"] = json!(true);
    result["retryable"] = json!(!ok && code != "AUTH_REQUIRED");
    result
}

async fn send(client: &Client, home: &Path, account: &str, renew: Renew) -> Result<()> {
    // Contract 1.65.0: a reading is as old as its request.
    let sent_at = crate::daemon::now();
    let sent = match renew {
        Renew::Allowed => oauth::authed(client, home, account, "GET", READ_PATH, None).await?,
        Renew::Never => {
            let token = oauth::current_token(home).with_context(|| {
                format!("AUTH_STALE: the stored Claude token of {account} is due for renewal; an on-demand read (api usage) or a running session renews it")
            })?;
            let sent = oauth::api(client, &token, "GET", READ_PATH, None).await;
            if matches!(&sent, Ok(r) if r.status == 401) {
                return Err(oauth::stale(
                    account,
                    "the API refused the stored token (HTTP 401); a background read never renews it",
                ));
            }
            sent
        }
    };
    let response = match sent {
        Ok(r) => r,
        Err(e @ (SendError::NotSent(_) | SendError::NoAnswer(_))) => {
            anyhow::bail!("USAGE_READ_FAILED: the Claude usage endpoint gave {e}; retry")
        }
    };
    ensure!(
        response.status == 200,
        "USAGE_READ_FAILED: the Claude usage endpoint answered HTTP {}; retry",
        response.status
    );
    let body: Value = serde_json::from_str(&response.body)
        .context("USAGE_READ_FAILED: the usage answer is not JSON")?;
    ensure!(
        body.is_object(),
        "USAGE_READ_FAILED: the usage answer is not a JSON object"
    );
    // Contract 1.65.0: the same answer refreshes the reset credits (`usage.reset_credits`) and
    // sees resets made elsewhere; a malformed `cedar_ember` block never fails the meter, a
    // window that could not be recorded does.
    crate::reset_credit::claude::absorb(home, &body, sent_at, None)
        .windows
        .map_err(|e| {
            if format!("{e:#}").starts_with("USAGE_READ_FAILED") {
                e
            } else {
                e.context("USAGE_READ_FAILED: the usage reading could not be recorded; retry")
            }
        })?;
    Ok(())
}

/// The newest time any window of the account's meter was learned.
pub fn newest_observation(home: &Path, now: i64) -> Option<i64> {
    let (usage, _, _) = super::project_usage(home, now);
    ["short", "weekly"]
        .iter()
        .filter_map(|k| usage[*k]["observed_at"].as_i64())
        .max()
}

fn refresh_seconds() -> i64 {
    std::env::var(REFRESH_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(REFRESH_SECONDS)
}

/// The supervisor's background read of idle Claude meters (one account at a time).
pub async fn monitor(paths: Paths) {
    let every = refresh_seconds();
    if every <= 0 {
        return;
    }
    loop {
        tokio::time::sleep(crate::timing::secs(every.clamp(1, 60) as u64)).await;
        let _ = tick(&paths, every).await;
    }
}
async fn tick(paths: &Paths, every: i64) -> Result<()> {
    let state = crate::store::load_state_snapshot(paths)?;
    let client = crate::reset_credit::claude::client(paths)?;
    for a in state.ordered() {
        if a.provider != ProviderId::Claude
            || !a.enabled
            || a.auth_required
            || a.deletion_id.is_some()
        {
            continue;
        }
        let Ok(home) = crate::provider::account_home(paths, a) else {
            continue;
        };
        let now = crate::daemon::now();
        // Contract 1.65.0: the reset credits are refreshed with the meter, so an account whose
        // meter a running session keeps fresh is still read when its status is old.
        let credits_young = crate::reset_credit::claude::cached(&home)
            .is_some_and(|(at, _)| (0..every).contains(&(now - at)));
        let attempted = last_attempt(&home).and_then(|l| l["attempted_at"].as_i64());
        // Contract 1.65.0: a recorded window reset that passed after the last attempt is read
        // at once (one read per reset), so the new cycle is shown within the fresh bound.
        let reset_due = super::recorded_resets(&home, now)
            .into_iter()
            .any(|r| r <= now && attempted.is_none_or(|at| at < r));
        if !reset_due {
            if credits_young && newest_observation(&home, now).is_some_and(|t| now - t < every) {
                continue;
            }
            // A failing account is retried once per period, not every minute.
            if attempted.is_some_and(|at| (0..every).contains(&(now - at))) {
                continue;
            }
        }
        read(
            &client,
            &home,
            &format!("claude:{}", a.name),
            true,
            Renew::Never,
        )
        .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
