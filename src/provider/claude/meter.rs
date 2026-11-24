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
mod tests {
    use super::*;

    #[test]
    fn usage_answer_windows_become_oauth_readings_that_never_lower_a_cycle() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let now = 1_800_000_000;
        // 2027-01-15T08:00:00Z and 2027-01-20T08:00:00Z, with a fraction and an offset.
        let (r5, r7) = (1_800_000_000 + 3_600, 1_800_000_000 + 86_400 * 5);
        let iso = |t: i64, frac: &str| {
            let days = t / 86_400;
            let secs = t % 86_400;
            // Civil date from days (Howard Hinnant), as the parser's inverse.
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y + 1 } else { y };
            format!(
                "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{frac}+00:00",
                secs / 3600,
                secs % 3600 / 60,
                secs % 60
            )
        };
        let body = |five: f64, week: f64| {
            json!({"five_hour":{"utilization":five,"resets_at":iso(r5, ".482910")},
                "seven_day":{"utilization":week,"resets_at":iso(r7, "")},
                "seven_day_opus":null,"extra_usage":{"is_enabled":false}})
        };
        assert!(record_answer(home, &body(31., 55.), now).unwrap());
        let (usage, fresh, _) = super::super::project_usage(home, now + 1);
        assert!(fresh);
        assert_eq!(usage["short"]["used_percent"], 31.0);
        assert_eq!(usage["short"]["resets_at"], r5);
        assert_eq!(usage["short"]["source"], "oauth");
        assert_eq!(usage["weekly"]["used_percent"], 55.0);
        assert_eq!(usage["weekly"]["resets_at"], r7);
        // A statusline replay of the same cycle with less usage never lowers it, even with
        // the header's whole-second reset time.
        let replay = json!({"session_id":"s","version":"2.1.314","rate_limits":{
            "five_hour":{"used_percentage":20.0,"resets_at":r5},
            "seven_day":{"used_percentage":56.5,"resets_at":r7 + 1}}});
        super::super::record_statusline(home, &replay, now + 60).unwrap();
        let (usage, _, _) = super::super::project_usage(home, now + 61);
        assert_eq!(usage["short"]["used_percent"], 31.0);
        assert_eq!(usage["short"]["source"], "oauth");
        assert_eq!(usage["short"]["observed_at"], now);
        assert_eq!(
            usage["weekly"]["used_percent"], 56.5,
            "a higher reading wins"
        );
        assert_eq!(usage["weekly"]["source"], "statusline");
        assert_eq!(usage["weekly"]["observed_at"], now + 60);
        // And an older OAuth answer never lowers what the statusline reported.
        assert!(!record_answer(home, &body(30., 50.), now + 120).unwrap());
        let (usage, _, _) = super::super::project_usage(home, now + 121);
        assert_eq!(usage["weekly"]["used_percent"], 56.5);
        assert_eq!(usage["short"]["used_percent"], 31.0);
        // A new cycle (a reset hours later) replaces with a lower value.
        let next = json!({"five_hour":{"utilization":2.0,"resets_at":iso(r5 + 18_000, "")}});
        assert!(record_answer(home, &next, now + 3_700).unwrap());
        let (usage, _, _) = super::super::project_usage(home, now + 3_701);
        assert_eq!(usage["short"]["used_percent"], 2.0);
        assert_eq!(usage["weekly"]["used_percent"], 56.5, "carried forward");
        // Null utilization or reset time is nothing observed; over 100% is at the limit.
        let w = windows(
            &json!({"five_hour":{"utilization":null,"resets_at":iso(r5, "")},
            "seven_day":{"utilization":104.0,"resets_at":iso(r7, "")}}),
        );
        assert!(w["five_hour"].is_null());
        assert_eq!(w["seven_day"]["used_percentage"], 100.0);
        assert!(!record_answer(home, &json!({"extra_usage":null}), now).unwrap());
        assert!(record_answer(home, &json!([]), now).is_err());
    }

    /// Contract 1.65.0: a reset no read saw heals. One lower OAuth answer in a closed cycle can
    /// be stale and is kept out; a second one at least CONFIRM_SECONDS later with the same or
    /// less is accepted as a `provider_lowered` cycle.
    #[test]
    fn a_confirmed_lower_oauth_reading_heals_a_missed_reset_and_a_single_one_does_not() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let now = 1_800_000_000;
        let r7 = now + 86_400 * 5;
        let iso = iso_z;
        let body = |week: f64| json!({"seven_day":{"utilization":week,"resets_at":iso(r7)}});
        let replay = |w: f64| {
            json!({"session_id":"s","version":"2.1.314","rate_limits":{
                "seven_day":{"used_percentage":w,"resets_at":r7}}})
        };
        super::super::record_statusline(home, &replay(20.0), now).unwrap();
        let week = |at: i64| super::super::project_usage(home, at).0["weekly"].clone();
        let natural = week(now)["cycle"].clone();
        // A single lower answer: kept out (usage.json is not rewritten).
        let before = std::fs::read(super::super::usage_file(home)).unwrap();
        assert!(!record_answer(home, &body(10.0), now + 10).unwrap());
        assert_eq!(
            std::fs::read(super::super::usage_file(home)).unwrap(),
            before
        );
        assert_eq!(week(now + 11)["used_percent"], 20.0);
        // Too soon is no confirmation.
        record_answer(home, &body(10.0), now + 30).unwrap();
        assert_eq!(week(now + 31)["used_percent"], 20.0);
        // Confirmed: accepted, a new cycle.
        assert!(record_answer(home, &body(10.0), now + 10 + CONFIRM).unwrap());
        let w = week(now + 11 + CONFIRM);
        assert_eq!(w["used_percent"], 10.0);
        assert_eq!(w["cycle_reason"], "provider_lowered");
        assert_ne!(w["cycle"], natural);
        // The stale value replayed by an idle session stays out afterwards.
        super::super::record_statusline(home, &replay(20.0), now + 200).unwrap();
        assert_eq!(week(now + 201)["used_percent"], 10.0);
    }
    const CONFIRM: i64 = crate::provider::usage_cycle::CONFIRM_SECONDS;
    /// RFC 3339 (`Z`) of Unix seconds (Howard Hinnant's civil date).
    fn iso_z(t: i64) -> String {
        let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )
    }
}
