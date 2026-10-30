//! Read-only CLI projections of the same data used by the dashboard.
use crate::{
    daemon, external,
    paths::Paths,
    usage_history::{HALF_HOUR, RETENTION},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub fn account(status: &Value, provider: &str, name: &str) -> Result<Value> {
    let account = status["accounts"]
        .as_array()
        .and_then(|accounts| {
            accounts.iter().find(|a| {
                a["name"] == name && a["provider"].as_str().unwrap_or("codex") == provider
            })
        })
        .context("unknown account")?;
    let remaining = |window: &str| {
        account["usage"][window]["used_percent"]
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
            .map(|n| 100.0 - n)
    };
    Ok(
        json!({"schema_version":1,"observed_at":status["observed_at"],
        "account":account,"remaining_percent":{"short":remaining("short"),"weekly":remaining("weekly")}}),
    )
}

pub fn history_range(
    now: i64,
    hours: u32,
    start: Option<i64>,
    end: Option<i64>,
    step: Option<i64>,
) -> Result<(i64, i64, i64)> {
    let end = end.unwrap_or_else(|| (now / HALF_HOUR + 1) * HALF_HOUR);
    ensure!(
        (0..=i64::MAX - RETENTION).contains(&end),
        "invalid history end"
    );
    let start = start.unwrap_or_else(|| (end - i64::from(hours) * 3600).max(0));
    ensure!(
        start >= 0 && end > start && end <= i64::MAX - RETENTION && end - start <= RETENTION,
        "invalid history range: choose a positive range of at most 90 days"
    );
    let step = step.unwrap_or_else(|| {
        ((end - start + 28 * HALF_HOUR - 1) / (28 * HALF_HOUR)).max(1) * HALF_HOUR
    });
    ensure!(
        (HALF_HOUR..=RETENTION).contains(&step)
            && step % HALF_HOUR == 0
            && (end - start + step - 1) / step <= 168,
        "invalid history step: use half-hour multiples and at most 168 bins"
    );
    Ok((start, end, step))
}

pub async fn session(paths: &Paths, key: &str) -> Result<Value> {
    // Explicit inventory identifiers only; never resolve display names.
    ensure!(
        ["pane:", "thread:", "tmux:", "archive:"]
            .iter()
            .any(|prefix| key.starts_with(prefix)),
        "use an exact session inventory ID, not a display name"
    );
    let snapshot = if key.starts_with("archive:") {
        external::opening::archives_snapshot(paths)?
    } else {
        external::tmux_inventory::snapshot_with_tmux(paths, true).await?
    };
    let matches: Vec<_> = snapshot["sessions"]
        .as_array()
        .context("invalid inventory")?
        .iter()
        .filter(|s| {
            s["id"] == key
                || key
                    .strip_prefix("thread:")
                    .is_some_and(|id| s["thread_id"] == id)
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "session unavailable or ambiguous; refresh sessions"
    );
    let session = matches[0];
    // Session discovery still works when the monitor is down; do not launch it.
    let status = daemon::call(paths, "status", json!({})).await.ok();
    let name = session["display_account"]
        .as_str()
        .or_else(|| session["account"].as_str())
        .unwrap_or_default();
    let provider = session["provider"].as_str().unwrap_or("codex");
    let account = status
        .as_ref()
        .and_then(|s| account(s, provider, name).ok());
    let thread = session["thread_id"].as_str().unwrap_or_default();
    Ok(
        json!({"schema_version":1,"observed_at":snapshot["observed_at"],"session":session,
        "account_details":account,"monitor_available":status.is_some(),
        "auto_switch":status.as_ref().and_then(|s|s["automatic_switch_preferences"].get(thread)),
        "auto_start":status.as_ref().and_then(|s|s["automatic_start_preferences"].get(thread))}),
    )
}

/// `display (name)` when a display name differs from the stable name.
pub fn account_label(a: &Value) -> String {
    let name = a["name"].as_str().unwrap_or("?");
    match a["display_name"].as_str() {
        Some(display) if display != name => format!("{display} ({name})"),
        _ => name.to_owned(),
    }
}
/// Human account listing across providers. Unknown usage shows `-`, never 0%.
pub fn account_table(accounts: &[Value]) -> String {
    let percent = |a: &Value, window: &str| {
        a["usage"][window]["used_percent"]
            .as_f64()
            .map_or("-".to_owned(), |p| format!("{p:.0}%"))
    };
    let mut out = format!(
        "{:<24} {:<8} {:<12} {:>5} {:>5} {}\n",
        "NAME", "PROVIDER", "STATUS", "5H", "WEEK", "USAGE"
    );
    for a in accounts {
        let status = if a["membership"] == "removed" {
            "removed".to_owned()
        } else if a["enabled"] != true {
            "disabled".to_owned()
        } else if a["auth_required"] == true {
            a["auth_detail"]
                .as_str()
                .map_or("login".to_owned(), str::to_ascii_lowercase)
        } else {
            "ready".to_owned()
        };
        out.push_str(&format!(
            "{:<24} {:<8} {:<12} {:>5} {:>5} {}\n",
            account_label(a),
            a["provider"].as_str().unwrap_or("?"),
            status,
            percent(a, "short"),
            percent(a, "weekly"),
            a["usage_state"].as_str().unwrap_or("unknown"),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_remaining_preserves_freshness_and_unknown_windows() {
        let s = json!({"observed_at":123,"accounts":[{"name":"work","usage_fresh":false,"usage":{"weekly":{"used_percent":42},"short":null}}]});
        let v = account(&s, "codex", "work").unwrap();
        assert_eq!(v["remaining_percent"]["weekly"], 58.0);
        assert!(v["remaining_percent"]["short"].is_null());
        assert_eq!(v["account"]["usage_fresh"], false);
        assert!(account(&s, "codex", "missing").is_err());
    }
    #[test]
    fn history_ranges_match_ui_and_reject_unbounded_requests() {
        let (s, e, step) = history_range(180000, 12, None, None, None).unwrap();
        assert_eq!(e - s, 12 * 3600);
        assert_eq!(step, 1800);
        let (s, e, step) = history_range(900000, 168, None, None, None).unwrap();
        assert_eq!(e - s, 168 * 3600);
        assert_eq!(step, 21600);
        assert!(history_range(0, 12, Some(-1), Some(2), None).is_err());
        assert!(history_range(0, 12, Some(0), Some(i64::MAX), None).is_err());
        assert!(history_range(0, 12, None, Some(i64::MIN), None).is_err());
        assert!(history_range(0, 12, Some(0), Some(RETENTION + 1), None).is_err());
        assert!(history_range(900000, 168, None, None, Some(1)).is_err());
        assert!(history_range(900000, 168, None, None, Some(1800)).is_err());
    }
}
