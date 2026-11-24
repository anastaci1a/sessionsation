//! Claude limit resets (contract 1.60.0): the Claude adapter of `account_reset_credit`.
//!
//! Claude Code 2.1.314 calls these "cedar-ember" (`/limit-reset [fable|full|5-hour|weekly]`):
//!
//! - status: `GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`; the
//!   answer's `cedar_ember` block lists grants (`{id, resets_left, paused, ends_at, type, ...}`);
//! - claim: `POST /api/organizations/<organizationUuid>/reset_rate_limits` with
//!   `{program:"cedar_ember", grant_id, request_id}`; the answer's `result` is one of `reset`,
//!   `already_used`, `not_limited`, `cooldown`, `ineligible`, `unavailable`.
//!
//! Both use the account's OAuth bearer ([`crate::provider::claude::oauth`]). The claim uses the
//! shared journal of [`crate::reset_credit`]: journaled before it is sent, the request id is
//! the backend `request_id` (its idempotency key), never retried with a new id.
use super::{admit, finish, read as read_record, validate_request_id};
use crate::{
    model::{ResetCredit, ResetCredits},
    paths::{Paths, atomic_write},
    provider::{
        ProviderId,
        claude::oauth::{self, Client, SendError},
    },
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const PROGRAM: &str = "cedar_ember";
pub const STATUS_PATH: &str = "/api/oauth/usage?cedar_ember=1&skip_spend=1";
/// A status read is reused for this long. Since 1.65.0 the supervisor's meter read (every
/// [`crate::provider::claude::meter::REFRESH_SECONDS`]) is this request too.
pub const CACHE_SECONDS: i64 = 60;
pub const API_LABEL: &str = "claude api reset_rate_limits (cedar_ember)";
/// Every outcome of a Claude claim. `cooldown` is Claude-only (nothing spent; claim later).
pub const OUTCOMES: &[&str] = &[
    "reset",
    "nothingToReset",
    "noCredit",
    "alreadyRedeemed",
    "cooldown",
];
/// Claude Code's grant kinds, in its order (`Xje`).
pub const KINDS: &[&str] = &["session", "weekly", "fable", "full"];

/// Claude Code's grant id rule (`x`).
pub fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
/// Claude Code's request id rule (`le`); a lowercase UUID passes.
pub fn valid_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
/// `5-hour` (also `5h`, `five-hour`), `weekly`, `fable` or `full` -> a grant kind.
pub fn parse_kind(text: &str) -> Result<&'static str> {
    let t = text.trim().to_ascii_lowercase();
    if matches!(t.as_str(), "5-hour" | "5h" | "five-hour" | "session") {
        return Ok("session");
    }
    KINDS.iter().find(|k| **k == t).copied().with_context(|| {
        format!("INVALID_ARGUMENT: --kind must be 5-hour, weekly, fable or full (got {text:?})")
    })
}

/// Unix seconds of an RFC 3339 time (`2026-11-27T05:00:00Z`, fractions and offsets allowed).
pub fn parse_time(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = text.get(r)?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if b[13] != b':' || b[16] != b':' || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    if h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) && &r[3..4] == ":" => {
            let oh: i64 = r[1..3].parse().ok()?;
            let om: i64 = r[4..6].parse().ok()?;
            let v = oh * 3600 + om * 60;
            if r.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s - offset)
}

fn time_field(v: &Value) -> Option<i64> {
    v.as_str().and_then(parse_time)
}
fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
/// Claude Code's grant kind (`qe`).
fn grant_kind(g: &Value, clears: &[String]) -> &'static str {
    if g["gives_back_weekly"].as_bool() == Some(true) {
        return "fable";
    }
    if let Some(t) = g["type"]
        .as_str()
        .and_then(|t| KINDS.iter().find(|k| **k == t))
    {
        return t;
    }
    let five = clears.iter().any(|c| c == "five_hour");
    let other = clears.iter().any(|c| c != "five_hour");
    match (five, other) {
        (true, false) => "session",
        (false, true) => "weekly",
        _ => "full",
    }
}
/// One grant (`me`/`he`); a malformed grant is skipped, as Claude Code does.
fn grant(g: &Value) -> Option<ResetCredit> {
    let id = g["id"].as_str().filter(|i| valid_grant_id(i))?;
    let left = g["resets_left"].as_u64()?;
    let paused = g["paused"].as_bool().unwrap_or(false);
    let clears = strings(&g["clears"]);
    Some(ResetCredit {
        id: id.into(),
        status: Some(
            if paused {
                "paused"
            } else if left > 0 {
                "available"
            } else {
                "used"
            }
            .into(),
        ),
        expires_at: time_field(&g["ends_at"]),
        kind: Some(grant_kind(g, &clears).into()),
        label: g["label"]
            .as_str()
            .filter(|l| !l.is_empty())
            .map(str::to_owned),
        resets_left: Some(left),
        resets_total: g["resets_total"].as_u64(),
        paused: Some(paused),
        usable_now: Some(g["usable_now"].as_bool().unwrap_or(false)),
        starts_at: time_field(&g["starts_at"]),
        clears: Some(clears),
        unavailable_reason: g["unavailable_reason"].as_str().map(str::to_owned),
    })
}

/// The `cedar_ember` block of a usage answer. `Ok(None)`: the block is absent (the account
/// is not offered resets). The body must be a JSON object; a block that is not one, or has
/// no boolean `eligible`, is malformed.
pub fn parse_status(body: &Value, at: i64) -> Result<Option<ResetCredits>> {
    ensure!(
        body.is_object(),
        "RESET_CREDITS_UNREADABLE: the usage answer is not a JSON object"
    );
    let block = &body[PROGRAM];
    if block.is_null() {
        return Ok(None);
    }
    let eligible = block["eligible"]
        .as_bool()
        .context("RESET_CREDITS_UNREADABLE: the cedar_ember block has no boolean eligible")?;
    let rows: Vec<ResetCredit> = block["grants"]
        .as_array()
        .map(|g| g.iter().filter_map(grant).collect())
        .unwrap_or_default();
    let available = rows
        .iter()
        .filter(|r| r.paused != Some(true))
        .map(|r| r.resets_left.unwrap_or(0))
        .sum();
    let next = block["next_grant_id"]
        .as_str()
        .filter(|id| rows.iter().any(|r| r.id == *id))
        .map(str::to_owned);
    Ok(Some(ResetCredits {
        available,
        observed_at: at,
        credits: Some(rows),
        eligible: Some(eligible),
        ineligible_reason: block["ineligible_reason"].as_str().map(str::to_owned),
        at_limit: Some(block["at_limit"].as_bool().unwrap_or(false)),
        next_grant_id: next,
        cooldown_until: time_field(&block["cooldown_until"]),
        weekly_resets_at: time_field(&block["weekly_resets_at"]),
    }))
}

// ------------------------------------------------------------------- cache

pub fn cache_file(home: &Path) -> PathBuf {
    home.join(".sessionsation/reset-credits.json")
}
/// The last status read: `(observed_at, reset_credits)`.
pub fn cached(home: &Path) -> Option<(i64, Option<ResetCredits>)> {
    let v: Value = serde_json::from_slice(&std::fs::read(cache_file(home)).ok()?).ok()?;
    let at = v["observed_at"].as_i64()?;
    let credits = serde_json::from_value(v["reset_credits"].clone()).ok()?;
    Some((at, credits))
}
/// The `usage.reset_credits` value of a Claude account row (null until a status read). Since
/// 1.65.0 it carries `fresh` and `age_seconds` like the meter (fresh: younger than the
/// Claude meter's staleness limit).
pub fn usage_field(home: &Path, now: i64) -> Value {
    cached(home).and_then(|(_, c)| c).map_or(Value::Null, |c| {
        crate::reset_credit::with_age(json!(c), now, crate::provider::claude::USAGE_STALE_SECONDS)
    })
}
/// The meter windows a reset of this grant clears. `clears` names limit windows
/// (`five_hour`, `seven_day`, `seven_day_overage_included`, ...); a `fable` grant gives back
/// weekly usage (`gives_back_weekly`) and a `full` or `weekly` one resets the week. Nothing
/// known: both windows, so a reset is never mistaken for a stale reading.
pub fn cleared_windows(clears: &[String], kind: Option<&str>) -> Vec<&'static str> {
    let mut out = vec![];
    let five = clears.iter().any(|c| c == "five_hour") || matches!(kind, Some("session" | "full"));
    let week = clears.iter().any(|c| c.starts_with("seven_day"))
        || matches!(kind, Some("weekly" | "fable" | "full"));
    if five {
        out.push("five_hour");
    }
    if week {
        out.push("seven_day");
    }
    if out.is_empty() {
        out = vec!["five_hour", "seven_day"];
    }
    out
}
/// The windows a grant of the stored status clears (both when the grant is unknown).
fn grant_windows(
    credits: Option<&ResetCredits>,
    grant: &str,
    cleared: &[String],
) -> Vec<&'static str> {
    let row = credits
        .and_then(|c| c.credits.as_deref())
        .and_then(|rows| rows.iter().find(|r| r.id == grant));
    let mut clears: Vec<String> = row.and_then(|r| r.clears.clone()).unwrap_or_default();
    clears.extend(cleared.iter().cloned());
    cleared_windows(&clears, row.and_then(|r| r.kind.as_deref()))
}
/// What one usage answer did: its windows (an error is a failed meter read) and its
/// `cedar_ember` status (an error is a malformed block; the windows are still recorded).
pub struct Absorbed {
    pub windows: Result<bool>,
    pub credits: Result<Option<ResetCredits>>,
}
/// One `/api/oauth/usage?cedar_ember=1` answer (contract 1.65.0: the status read and every
/// meter read are the same request), all under the recorder's lock (`usage.lock`), so two
/// reads never compare against the same stored status. `at` is when the request was sent. In
/// order: the grants that [`fallen`] finds are resets made elsewhere (Claude Code's
/// `/limit-reset`), so the windows they clear start a new cycle; then the answer's windows
/// are recorded as an OAuth reading (accepted in that new cycle even when lower); then the
/// status is stored. An answer sent before this account's own last claim was answered
/// ([`claimed`]) is older than the stored status: nothing is inferred or stored from it.
pub fn absorb(home: &Path, body: &Value, at: i64, own: Option<&str>) -> Absorbed {
    let lock = match crate::provider::claude::recorder_lock(home) {
        Ok(l) => l,
        Err(e) => {
            return Absorbed {
                windows: Err(anyhow::anyhow!("USAGE_READ_FAILED: {e:#}")),
                credits: Err(e),
            };
        }
    };
    let credits = parse_status(body, at);
    let stale = claimed_at(home).is_some_and(|t| at < t);
    if !stale && let Ok(Some(now)) = &credits {
        let listed = body[PROGRAM]["grants"].is_array();
        let windows = fallen(cached(home), now, listed, at, own);
        if !windows.is_empty() {
            // Best effort: a marking that fails leaves the old cycle, never the read.
            let _ = crate::provider::claude::start_cycles_locked(
                &lock,
                home,
                &windows,
                at,
                crate::provider::usage_cycle::EXTERNAL_REDEEM,
            );
        }
    }
    // Contract 1.62.0: the same answer carries the usage windows (an OAuth reading).
    let windows = crate::provider::claude::meter::record_answer_locked(&lock, home, body, at);
    let credits = credits.and_then(|c| {
        if !stale {
            store(home, at, &c)?;
        }
        Ok(c)
    });
    Absorbed { windows, credits }
}
/// The meter windows a reset made elsewhere cleared: a grant of the stored status (not
/// `own`) that had uses left and had not ended, and now has fewer, or is gone from a listed
/// `grants` array (a grant without `resets_left` is skipped as malformed, so it is gone too).
/// Nothing is inferred without a stored status, or for a window shorter than that status's
/// age: its natural reset has passed since, and the next reading starts a new cycle anyway.
fn fallen(
    before: Option<(i64, Option<ResetCredits>)>,
    now: &ResetCredits,
    listed: bool,
    at: i64,
    own: Option<&str>,
) -> Vec<&'static str> {
    let Some((then, Some(before))) = before else {
        return vec![];
    };
    let rows = now.credits.as_deref().unwrap_or_default();
    let mut out: Vec<&'static str> = vec![];
    for o in before.credits.as_deref().unwrap_or_default() {
        let Some(had) = o.resets_left.filter(|n| *n > 0) else {
            continue;
        };
        if own == Some(o.id.as_str()) || o.expires_at.is_some_and(|e| e <= at) {
            continue;
        }
        let fell = match rows.iter().find(|g| g.id == o.id) {
            Some(g) => g.resets_left.is_none_or(|n| n < had),
            None => listed,
        };
        if !fell {
            continue;
        }
        for w in grant_windows(Some(&before), &o.id, &[]) {
            let length = crate::provider::usage_cycle::WINDOWS
                .iter()
                .find(|(k, _)| *k == w)
                .map_or(0, |(_, d)| *d);
            if at - then <= length && !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}
/// This account's own claim of `grant` was answered `reset` at `at` (contract 1.65.0): under
/// the recorder's lock, the windows it clears start a new cycle and the stored status counts
/// the claim (`resets_left` from the answer, else one less, and `claimed_at`), so no read of
/// another process reports this grant as a reset made elsewhere, and a read sent before the
/// answer is not stored over it. Returns the windows whose cycle started.
pub fn claimed(
    home: &Path,
    grant: &str,
    left_after: Option<u64>,
    at: i64,
    cleared: &[String],
) -> Result<Vec<String>> {
    let lock = crate::provider::claude::recorder_lock(home)?;
    let stored = cached(home);
    let windows = grant_windows(
        stored.as_ref().and_then(|(_, s)| s.as_ref()),
        grant,
        cleared,
    );
    let started = crate::provider::claude::start_cycles_locked(
        &lock,
        home,
        &windows,
        at,
        crate::provider::usage_cycle::REDEEM,
    )?;
    let (then, mut credits) = stored.unwrap_or((at, None));
    if let Some(s) = credits.as_mut()
        && let Some(row) = s
            .credits
            .as_mut()
            .and_then(|rows| rows.iter_mut().find(|r| r.id == grant))
    {
        row.resets_left = left_after.or(row.resets_left.map(|n| n.saturating_sub(1)));
        if row.resets_left == Some(0) && row.paused != Some(true) {
            row.status = Some("used".into());
        }
        s.available = s
            .credits
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|r| r.paused != Some(true))
            .map(|r| r.resets_left.unwrap_or(0))
            .sum();
    }
    atomic_write(
        &cache_file(home),
        &serde_json::to_vec(&json!({"observed_at":then,"reset_credits":credits,"claimed_at":at}))?,
    )?;
    Ok(started)
}
/// When this account's own last claim was answered (see [`claimed`]).
fn claimed_at(home: &Path) -> Option<i64> {
    let v: Value = serde_json::from_slice(&std::fs::read(cache_file(home)).ok()?).ok()?;
    v["claimed_at"].as_i64()
}
fn store(home: &Path, at: i64, credits: &Option<ResetCredits>) -> Result<()> {
    atomic_write(
        &cache_file(home),
        &serde_json::to_vec(
            &json!({"observed_at":at,"reset_credits":credits,"claimed_at":claimed_at(home)}),
        )?,
    )
}

// ------------------------------------------------------------------- calls

/// The account's reset credits: the cached read when younger than [`CACHE_SECONDS`]
/// (unless `force`), else a status read, stored for `api account list` and `api state`.
pub async fn read(
    client: &Client,
    home: &Path,
    account: &str,
    force: bool,
) -> Result<Option<ResetCredits>> {
    read_after(client, home, account, force, None).await
}
/// [`read`]; `own` is a grant this process just claimed, whose falling count is not an
/// external reset.
async fn read_after(
    client: &Client,
    home: &Path,
    account: &str,
    force: bool,
    own: Option<&str>,
) -> Result<Option<ResetCredits>> {
    let now = crate::daemon::now();
    if !force
        && let Some((at, credits)) = cached(home)
        && (0..CACHE_SECONDS).contains(&(now - at))
    {
        return Ok(credits);
    }
    // Contract 1.65.0: a reading is as old as its request.
    let sent_at = crate::daemon::now();
    let response = match oauth::authed(client, home, account, "GET", STATUS_PATH, None).await? {
        Ok(r) => r,
        Err(e) => bail!("RESET_CREDITS_READ_FAILED: the Claude usage endpoint gave {e}; retry"),
    };
    ensure!(
        response.status == 200,
        "RESET_CREDITS_READ_FAILED: the Claude usage endpoint answered HTTP {}; retry",
        response.status
    );
    let body: Value = serde_json::from_str(&response.body)
        .context("RESET_CREDITS_UNREADABLE: the usage answer is not JSON")?;
    absorb(home, &body, sent_at, own).credits
}

/// The grant a claim without `--credit-id` uses: usable now, not paused, not ended, of
/// `kind` when given; else the backend's `next_grant_id`, else the first in kind order.
pub fn choose(credits: &ResetCredits, kind: Option<&str>, now: i64) -> Option<String> {
    let rows = credits.credits.as_deref().unwrap_or_default();
    let usable: Vec<&ResetCredit> = rows
        .iter()
        .filter(|r| {
            r.paused != Some(true)
                && r.usable_now == Some(true)
                && r.expires_at.is_none_or(|e| e > now)
                && kind.is_none_or(|k| r.kind.as_deref() == Some(k))
        })
        .collect();
    if kind.is_none()
        && let Some(next) = &credits.next_grant_id
        && usable.iter().any(|r| r.id == *next)
    {
        return Some(next.clone());
    }
    let order = |r: &&ResetCredit| {
        KINDS
            .iter()
            .position(|k| r.kind.as_deref() == Some(*k))
            .unwrap_or(KINDS.len())
    };
    usable.into_iter().min_by_key(order).map(|r| r.id.clone())
}

/// A claim answer (`be`) -> (phase, outcome). `retry`: this request id was sent before,
/// so `already_used` means the earlier send applied (Claude Code shows it as spent).
pub fn classify(result: &str, retry: bool) -> (&'static str, Option<&'static str>) {
    match result {
        "reset" => ("complete", Some("reset")),
        "not_limited" => ("complete", Some("nothingToReset")),
        "already_used" if retry => ("complete", Some("alreadyRedeemed")),
        "already_used" | "ineligible" => ("complete", Some("noCredit")),
        "cooldown" => ("complete", Some("cooldown")),
        // `unavailable` (and anything unknown, which Claude Code reads as it): unconfirmed.
        _ => ("uncertain", None),
    }
}

pub struct Claim<'a> {
    pub paths: &'a Paths,
    pub client: &'a Client,
    pub home: &'a Path,
    pub name: &'a str,
    pub credit_id: Option<String>,
    pub kind: Option<&'static str>,
    pub request_id: String,
}

/// `api account reset-credit claude:NAME`. Holds the account operation lock throughout.
pub async fn redeem(c: Claim<'_>) -> Result<Value> {
    let account = format!("claude:{}", c.name);
    validate_request_id(&c.request_id)?;
    ensure!(
        valid_request_id(&c.request_id),
        "INVALID_REQUEST_ID: --request-id must be a lowercase hyphenated UUID"
    );
    if let Some(g) = &c.credit_id {
        ensure!(
            valid_grant_id(g),
            "INVALID_ARGUMENT: a Claude --credit-id is 1-40 of a-z, 0-9, _ and -"
        );
    }
    let _lock = crate::accounts::operation_lock(c.paths, ProviderId::Claude, c.name)?;
    let org = oauth::organization_uuid(c.home).with_context(|| {
        format!("AUTH_REQUIRED: {account} has no oauthAccount.organizationUuid in .claude.json; log in again")
    })?;
    // A retry reuses the grant its first send named, so one request id never names two.
    let previous = super::exists(c.paths, &c.request_id)
        .then(|| read_record(c.paths, &c.request_id))
        .transpose()?;
    let grant = match (&previous, &c.credit_id) {
        (Some(r), Some(g)) if r.credit_id.as_deref() != Some(g) => bail!(
            "REQUEST_ID_CONFLICT: request {} was used for {} (credit {}); use a new request id for a different attempt",
            c.request_id,
            r.account_id,
            r.credit_id.as_deref().unwrap_or("any")
        ),
        (Some(r), _) => r.credit_id.clone().with_context(|| {
            format!(
                "REQUEST_ID_CONFLICT: request {} has no recorded grant",
                c.request_id
            )
        })?,
        (None, Some(g)) => g.clone(),
        (None, None) => {
            let status = read(c.client, c.home, &account, true).await?;
            let now = crate::daemon::now();
            match status.as_ref().and_then(|s| choose(s, c.kind, now)) {
                Some(g) => g,
                None => {
                    let after = crate::provider::claude::project_usage(c.home, now).0;
                    return Ok(
                        json!({"account_id":account,"request_id":c.request_id,"outcome":"noCredit",
                        "sent":false,"detail":format!("no {} grant is usable now; nothing was sent", c.kind.unwrap_or("reset")),
                        "rate_limits_after":after,"attempts":0,"cycles_started":[],"operation":Value::Null}),
                    );
                }
            }
        }
    };
    // Renew (when due) before journaling, so a login problem journals nothing.
    let token = oauth::access_token(c.client, c.home, &account, None).await?;
    let mut record = admit(
        c.paths,
        ProviderId::Claude,
        &account,
        Some(&grant),
        &c.request_id,
        API_LABEL,
    )?;
    // `already_used` confirms this request id only when an earlier send may have reached
    // the API (a refused or unsent first attempt claimed nothing).
    let retry = previous.as_ref().is_some_and(|r| r.phase != "failed");
    let path = format!("/api/organizations/{org}/reset_rate_limits");
    let body = json!({"program":PROGRAM,"grant_id":grant,"request_id":c.request_id});
    let mut sent = oauth::api(c.client, &token, "POST", &path, Some(&body)).await;
    if matches!(&sent, Ok(r) if r.status == 401) {
        // Refused before it was processed: the same request id again, once, renewed.
        match oauth::access_token(c.client, c.home, &account, Some(&token)).await {
            Ok(renewed) => {
                record.attempts += 1;
                sent = oauth::api(c.client, &renewed, "POST", &path, Some(&body)).await;
            }
            Err(e) => {
                finish(
                    c.paths,
                    &mut record,
                    "failed",
                    None,
                    "the API refused the token and it could not be renewed; nothing was claimed",
                )?;
                return Err(e);
            }
        }
    }
    let retry_hint = format!("Retry with --request-id {}", c.request_id);
    let response = match sent {
        Ok(r) => r,
        Err(SendError::NotSent(why)) => {
            let m = format!(
                "RESET_CREDIT_FAILED: the claim was not sent ({why}); nothing was claimed. {retry_hint}"
            );
            finish(c.paths, &mut record, "failed", None, &m)?;
            bail!("{m}");
        }
        Err(SendError::NoAnswer(why)) => {
            let m = format!(
                "RESET_CREDIT_OUTCOME_UNKNOWN: the claim got no answer ({why}); not retried. {retry_hint}: the same request id cannot claim twice"
            );
            finish(c.paths, &mut record, "uncertain", None, &m)?;
            bail!("{m}");
        }
    };
    match response.status {
        200 => {}
        401 => {
            let m = "the API refused the renewed token (HTTP 401); nothing was claimed";
            finish(c.paths, &mut record, "failed", None, m)?;
            return Err(oauth::stale(&account, m));
        }
        s if s == 429 || (400..500).contains(&s) => {
            let m = format!(
                "RESET_CREDIT_FAILED: the Claude API refused the claim (HTTP {s}); nothing was claimed; not retried. {retry_hint}"
            );
            finish(c.paths, &mut record, "failed", None, &m)?;
            bail!("{m}");
        }
        s => {
            let m = format!(
                "RESET_CREDIT_OUTCOME_UNKNOWN: the Claude API answered HTTP {s}; not retried. {retry_hint}: the same request id cannot claim twice"
            );
            finish(c.paths, &mut record, "uncertain", None, &m)?;
            bail!("{m}");
        }
    }
    let answer: Option<Value> = serde_json::from_str::<Value>(&response.body)
        .ok()
        .filter(|v| v["result"].is_string());
    let Some(answer) = answer else {
        let m = format!(
            "RESET_CREDIT_OUTCOME_UNKNOWN: the claim answer was unreadable; not retried. {retry_hint}: the same request id cannot claim twice"
        );
        finish(c.paths, &mut record, "uncertain", None, &m)?;
        bail!("{m}");
    };
    let result = answer["result"].as_str().unwrap_or_default().to_owned();
    let reason = answer["reason"].as_str().map(str::to_owned);
    record.provider_result = Some(match &reason {
        Some(r) => format!("{result}/{r}"),
        None => result.clone(),
    });
    let (phase, outcome) = classify(&result, retry);
    let Some(outcome) = outcome else {
        let m = format!(
            "RESET_CREDIT_OUTCOME_UNKNOWN: the Claude API could not confirm the claim (result {result}); not retried. {retry_hint}: the same request id cannot claim twice"
        );
        finish(c.paths, &mut record, phase, None, &m)?;
        bail!("{m}");
    };
    finish(
        c.paths,
        &mut record,
        phase,
        Some(outcome),
        &format!("the Claude API answered {result}"),
    )?;
    // Contract 1.65.0: the windows this grant clears start a new reset cycle now, so the
    // lower reading that follows is accepted (within a cycle usage never goes down).
    let mut cycles_started: Vec<String> = vec![];
    if outcome == "reset" {
        cycles_started = claimed(
            c.home,
            &grant,
            answer["resets_left"].as_u64(),
            crate::daemon::now(),
            &strings(&answer["cleared"]),
        )
        .unwrap_or_default();
    }
    // The rate limits after a claim are a fresh read.
    let mut after = Value::Null;
    let mut read_error = Value::Null;
    if matches!(outcome, "reset" | "alreadyRedeemed") {
        let own = (outcome == "reset").then_some(grant.as_str());
        match read_after(c.client, c.home, &account, true, own).await {
            Ok(_) => {
                after = crate::provider::claude::project_usage(c.home, crate::daemon::now()).0;
            }
            Err(e) => read_error = json!(format!("{e:#}")),
        }
    }
    let times = |k: &str| time_field(&answer[k]);
    Ok(
        json!({"account_id":account,"request_id":c.request_id,"outcome":outcome,"credit_id":grant,
        "rate_limits_after":after,"rate_limits_after_error":read_error,"attempts":record.attempts,
        "cycles_started":cycles_started,
        "claude":{"result":result,"reason":reason,"resets_left":answer["resets_left"].as_u64(),
            "cleared":strings(&answer["cleared"]),"weekly_resets_at":times("weekly_resets_at"),
            "cooldown_until":times("cooldown_until")},
        "operation":super::public(&record)}),
    )
}

/// The account row (from the supervisor) and its config home.
async fn account_home(paths: &Paths, name: &str) -> Result<(Value, PathBuf)> {
    crate::daemon::ensure_started(paths).await?;
    let status = crate::daemon::call(paths, "status", json!({})).await?;
    let row = status["accounts"]
        .as_array()
        .and_then(|rows| {
            rows.iter().find(|a| {
                a["provider"].as_str().is_some_and(|p| {
                    crate::provider::ops::holds(p, |f| {
                        f.reset_credits == crate::provider::ops::Reader::OAuth
                    })
                }) && a["name"] == name
            })
        })
        .cloned()
        .context("ACCOUNT_NOT_FOUND")?;
    ensure!(row["deletion_id"].is_null(), "ACCOUNT_DELETION_IN_PROGRESS");
    let home =
        crate::provider::claude::home(paths, name, row["config_home"].as_str().map(Path::new))?;
    Ok((row, home))
}
pub fn client(paths: &Paths) -> Result<Client> {
    Ok(Client::new(crate::store::load_config(paths)?.curl_binary))
}
/// `api account reset-credit claude:NAME ...`.
pub async fn command(
    paths: &Paths,
    name: &str,
    credit_id: Option<String>,
    kind: Option<&'static str>,
    request_id: String,
) -> Result<Value> {
    let (_, home) = account_home(paths, name).await?;
    let client = client(paths)?;
    redeem(Claim {
        paths,
        client: &client,
        home: &home,
        name,
        credit_id,
        kind,
        request_id,
    })
    .await
}
/// `api account limits claude:NAME`: a status read (cached [`CACHE_SECONDS`]). A failed
/// read keeps the last stored value and is reported, never fatal.
pub async fn limits_read(paths: &Paths, name: &str, home: &Path) -> (Value, Value) {
    let account = format!("claude:{name}");
    let read = async {
        let client = client(paths)?;
        read(&client, home, &account, false).await
    };
    match read.await {
        Ok(_) => (usage_field(home, crate::daemon::now()), json!({"ok":true})),
        Err(e) => {
            let text = format!("{e}");
            let code = text.split(':').next().unwrap_or_default();
            let code =
                if !code.is_empty() && code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
                    code
                } else {
                    "RESET_CREDITS_READ_FAILED"
                };
            (
                usage_field(home, crate::daemon::now()),
                json!({"ok":false,"code":code,"message":text,"retryable":code == "AUTH_STALE" || code == "RESET_CREDITS_READ_FAILED"}),
            )
        }
    }
}
#[cfg(test)]
mod tests;
