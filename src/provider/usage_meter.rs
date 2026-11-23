//! One account usage meter shape for every provider (contract 1.62.0, SM8).
//!
//! `usage.meter` of every account row (`api account list`, `api account limits`, `api state`)
//! and of every `api usage` row:
//!
//! ```json
//! {"supported":true,"requires_session":false,"source":"oauth","fresh":true,
//!  "observed_at":1800000000,"age_seconds":12,
//!  "windows":{"five_hour":{"used_percent":31.0,"resets_at":1800003600,"observed_at":1800000000,"source":"oauth"},
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
//! reset, and the newest reading is younger than the provider's staleness limit.
use crate::provider::{ProviderId, claude};
use serde_json::{Value, json};

pub const GEMINI_REASON: &str = "the Gemini CLI reports no subscription usage windows to read";
pub const ANTIGRAVITY_REASON: &str =
    "the Antigravity CLI reports no subscription usage windows to read";

fn unsupported(reason: &str) -> Value {
    json!({"supported":false,"requires_session":Value::Null,"source":Value::Null,"fresh":false,
        "observed_at":Value::Null,"age_seconds":Value::Null,
        "windows":{"five_hour":Value::Null,"seven_day":Value::Null},"reason":reason})
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
    let window = |key: &str| {
        let w = &usage[key];
        let (Some(used), Some(resets)) = (w["used_percent"].as_f64(), w["resets_at"].as_i64())
        else {
            return Value::Null;
        };
        json!({"used_percent":used,"resets_at":resets,
            "observed_at":w["observed_at"].as_i64().or(top),
            "source":w["source"].as_str().unwrap_or(default_source)})
    };
    let (five, seven) = (window("short"), window("weekly"));
    let present: Vec<&Value> = [&five, &seven]
        .into_iter()
        .filter(|w| !w.is_null())
        .collect();
    let newest = present
        .iter()
        .filter_map(|w| w["observed_at"].as_i64().map(|t| (t, w["source"].clone())))
        .max_by_key(|(t, _)| *t);
    let observed_at = newest.as_ref().map(|(t, _)| (*t).min(now)).or(top);
    let fresh = !present.is_empty()
        && present
            .iter()
            .all(|w| w["resets_at"].as_i64().is_some_and(|r| r > now))
        && observed_at.is_some_and(|t| now - t < limit);
    json!({"supported":true,"requires_session":false,
        "source":newest.map_or(if top.is_some() {json!(default_source)} else {Value::Null}, |(_, s)| s),
        "fresh":fresh,"observed_at":observed_at,
        "age_seconds":observed_at.map(|t| (now - t).max(0)),
        "windows":{"five_hour":five,"seven_day":seven},
        "reason":if present.is_empty() {json!("no usage window has been read yet")} else {Value::Null}})
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
mod tests {
    use super::*;

    #[test]
    fn every_provider_has_the_same_meter_keys_and_nothing_is_invented() {
        let now = 1_000_000;
        let codex = json!({"short":{"used_percent":23.0,"duration_minutes":300,"resets_at":now+100},
            "weekly":{"used_percent":42.0,"duration_minutes":10080,"resets_at":now+1000},"observed_at":now-10});
        let claude = json!({"short":{"used_percent":31.0,"resets_at":now+100,"observed_at":now-50,"source":"oauth"},
            "weekly":{"used_percent":56.5,"resets_at":now+1000,"observed_at":now-5,"source":"statusline"},
            "observed_at":now-5});
        let keys = |v: &Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let rows = [
            of(ProviderId::Codex, &codex, now, 300),
            of(ProviderId::Claude, &claude, now, 300),
            of(ProviderId::Gemini, &json!({}), now, 300),
            of(ProviderId::Antigravity, &json!({}), now, 300),
            of(
                ProviderId::Claude,
                &json!({"short":null,"weekly":null,"observed_at":null}),
                now,
                300,
            ),
        ];
        for r in &rows {
            assert_eq!(keys(r), keys(&rows[0]));
        }
        let c = &rows[0];
        assert_eq!(c["source"], "app_server");
        assert_eq!(c["requires_session"], false);
        assert_eq!(c["fresh"], true);
        assert_eq!(c["age_seconds"], 10);
        assert_eq!(c["windows"]["five_hour"]["used_percent"], 23.0);
        let c = &rows[1];
        assert_eq!(c["source"], "statusline", "the newest window's source");
        assert_eq!(c["observed_at"], now - 5);
        assert_eq!(c["windows"]["five_hour"]["source"], "oauth");
        assert_eq!(rows[2]["supported"], false);
        assert!(rows[2]["reason"].is_string());
        assert_eq!(rows[4]["fresh"], false);
        assert!(rows[4]["windows"]["five_hour"].is_null());
        assert!(rows[4]["source"].is_null());
        // Past a reset, or too old: not fresh.
        assert_eq!(
            of(ProviderId::Codex, &codex, now + 200, 300)["fresh"],
            false
        );
        assert_eq!(of(ProviderId::Codex, &codex, now + 50, 30)["fresh"], false);
    }
}
