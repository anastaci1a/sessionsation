//! One account usage meter shape for every provider (contract 1.62.0, SM8).
//!
//! `usage.meter` of every account row (`api account list`, `api account limits`, `api state`)
//! and of every `api usage` row:
//!
//! ```json
//! {"supported":true,"requires_session":false,"source":"oauth","fresh":true,
//!  "observed_at":1800000000,"age_seconds":12,
//!  "windows":{"five_hour":{"used_percent":31.0,"resets_at":1800003600,"observed_at":1800000000,"source":"oauth",
//!                           "cycle":"1800003600:1799985600:natural","cycle_started_at":1799985600,"cycle_reason":"natural"},
//!             "seven_day":null},
//!  "reason":null}
//! ```
//!
//! - Codex: the supervisor's `account/rateLimits/read` of every enabled account (`source`
//!   `app_server`); no session needed.
//! - Claude: the statusline recorder and the OAuth usage read share one owner
//!   ([`crate::provider::claude::record_windows`]); every window names where it was learned
//!   (`statusline` or `oauth`) and `source` is the newest window's. No session needed.
//! - Gemini and Antigravity: `supported:false` with the reason; nothing is invented.
//!
//! A missing window is `null`, never 0%. `fresh`: some window is present, none is past its
//! reset, and the newest reading is younger than the provider's staleness limit. Each present
//! window also has its own `fresh` and `age_seconds` (its own `observed_at` against the same
//! limit; past its reset is never fresh); `all_windows_fresh` is true when every present
//! window is fresh.
use crate::provider::{ProviderId, claude, usage_cycle};
use serde_json::{Value, json};

pub const GEMINI_REASON: &str = "the Gemini CLI reports no subscription usage windows to read";
pub const ANTIGRAVITY_REASON: &str =
    "the Antigravity CLI reports no subscription usage windows to read";

fn unsupported(reason: &str) -> Value {
    json!({"supported":false,"requires_session":Value::Null,"source":Value::Null,"fresh":false,"all_windows_fresh":false,
        "observed_at":Value::Null,"age_seconds":Value::Null,
        "windows":{"five_hour":Value::Null,"seven_day":Value::Null},
        "window_reasons":{"five_hour":reason,"seven_day":reason},"reason":reason})
}

/// The meter of one account from its projected `usage` (the shared account shape: `short` /
/// `weekly` windows with `used_percent`, `resets_at` and, for Claude, per-window
/// `observed_at` and `source`). `stale_seconds` is the Codex staleness limit.
pub fn of(provider: ProviderId, usage: &Value, now: i64, stale_seconds: u64) -> Value {
    let (limit, default_source) = match provider {
        ProviderId::Gemini => return unsupported(GEMINI_REASON),
        ProviderId::Antigravity => return unsupported(ANTIGRAVITY_REASON),
        ProviderId::Codex => (stale_seconds as i64, "app_server"),
        ProviderId::Claude => (claude::USAGE_STALE_SECONDS, "statusline"),
    };
    let top = usage["observed_at"].as_i64().filter(|t| *t > 0);
    let window = |key: &str, duration_seconds: i64| {
        let w = &usage[key];
        // Contract 1.65.0: a window that has not started (0%, no reset time) is current.
        let first = w["starts_on_first_use"] == true;
        let resets = w["resets_at"].as_i64();
        let Some(used) = w["used_percent"]
            .as_f64()
            .filter(|_| first || resets.is_some())
        else {
            return Value::Null;
        };
        let mut m = json!({"used_percent":used,"resets_at":resets,
            "observed_at":w["observed_at"].as_i64().or(top),
            "source":w["source"].as_str().unwrap_or(default_source),
            "starts_on_first_use":first,
            // Contract 1.65.0: the reset cycle (see provider::usage_cycle).
            "cycle":w["cycle"],"cycle_started_at":w["cycle_started_at"],"cycle_reason":w["cycle_reason"]});
        usage_cycle::fill(&mut m, duration_seconds);
        // Contract 1.67.0: the window's own freshness, from its own observed_at and reset.
        let seen = m["observed_at"].as_i64().map(|t| t.min(now));
        let past = resets.is_some_and(|r| r <= now) && !first;
        m["age_seconds"] = json!(seen.map(|t| (now - t).max(0)));
        m["fresh"] = json!(!past && seen.is_some_and(|t| now - t < limit));
        m
    };
    let (five, seven) = (
        window("short", usage_cycle::WINDOWS[0].1),
        window("weekly", usage_cycle::WINDOWS[1].1),
    );
    // Contract 1.65.0: every window that is null or past its reset says why.
    let mut window_reasons = json!({});
    for ((key, _), w) in usage_cycle::WINDOWS.iter().zip([&five, &seven]) {
        window_reasons[*key] = match w["resets_at"].as_i64() {
            Some(r) if r <= now => json!(reset_reason(r)),
            _ if w.is_null() => usage["window_reasons"][*key]
                .as_str()
                .map(|r| json!(r))
                .unwrap_or_else(|| {
                    json!(if top.is_some() {
                        "the provider's last reading has no current value for this window"
                    } else {
                        "no usage window has been read yet"
                    })
                }),
            _ => Value::Null,
        };
    }
    let present: Vec<&Value> = [&five, &seven]
        .into_iter()
        .filter(|w| !w.is_null())
        .collect();
    let newest = present
        .iter()
        .filter_map(|w| w["observed_at"].as_i64().map(|t| (t, w["source"].clone())))
        .max_by_key(|(t, _)| *t);
    let observed_at = newest.as_ref().map(|(t, _)| (*t).min(now)).or(top);
    // A window past its reset is never fresh: it waits for a read after the reset.
    let past = present.iter().find_map(|w| {
        w["resets_at"]
            .as_i64()
            .filter(|r| *r <= now && w["starts_on_first_use"] != true)
    });
    let fresh =
        !present.is_empty() && past.is_none() && observed_at.is_some_and(|t| now - t < limit);
    // Contract 1.67.0: every present window is itself fresh (a null window is not counted).
    let all_windows_fresh = !present.is_empty() && present.iter().all(|w| w["fresh"] == true);
    let reason = if present.is_empty() {
        json!("no usage window has been read yet")
    } else if let Some(r) = past {
        json!(reset_reason(r))
    } else if !fresh && provider == ProviderId::Claude {
        // Codex says why in daemon::explain_stale_meter (lease, terminal, failed read).
        json!(format!(
            "the last reading is {} s old and no newer one has been recorded",
            observed_at.map_or(0, |t| (now - t).max(0))
        ))
    } else {
        Value::Null
    };
    json!({"supported":true,"requires_session":false,
        "source":newest.map_or(if top.is_some() {json!(default_source)} else {Value::Null}, |(_, s)| s),
        "fresh":fresh,"all_windows_fresh":all_windows_fresh,"observed_at":observed_at,
        "age_seconds":observed_at.map(|t| (now - t).max(0)),
        "windows":{"five_hour":five,"seven_day":seven},
        "window_reasons":window_reasons,
        "reason":reason})
}
/// Why a window past its own reset is not current (contract 1.65.0).
pub fn reset_reason(resets_at: i64) -> String {
    format!("window reset at {resets_at}; awaiting a post-reset read")
}

/// Fill `usage.meter` of an account row that has none (a row from an older supervisor, or a
/// cached snapshot whose usage was just replaced).
pub fn ensure(row: &mut Value, now: i64, stale_seconds: u64) {
    if !row["usage"].is_object() || row["usage"]["meter"].is_object() {
        return;
    }
    let Ok(provider) = ProviderId::parse(row["provider"].as_str().unwrap_or("codex")) else {
        return;
    };
    row["usage"]["meter"] = of(provider, &row["usage"], now, stale_seconds);
}

#[cfg(test)]
mod tests;
