//! Content-free live activity of a running Codex session, shared by `api state`
//! (and its cached snapshot) and the send readiness gate. Facts come from the
//! app-server thread status when it was read and otherwise from a bounded
//! rollout tail; no message text is ever included, only the provider's own
//! short turn-error text (for example "Selected model is at capacity").
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Longest error text published; longer provider text is cut at a char boundary.
pub const ERROR_TEXT_MAX: usize = 300;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnFacts {
    /// Any evidence at all (rollout events or an app-server thread status).
    #[serde(default)]
    pub observed: bool,
    /// Where the newest evidence came from: `rollout` or `app_server`.
    #[serde(default)]
    pub source: Option<String>,
    /// A turn has been seen in the observed window or reported by the provider.
    #[serde(default)]
    pub started: bool,
    #[serde(default)]
    pub in_progress: bool,
    /// Waiting on an approval or user input (provider `activeFlags`).
    #[serde(default)]
    pub blocked: bool,
    /// `completed`, `failed` or `interrupted` for the latest finished turn.
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Bound provider error text: one line of printable characters.
pub fn clean_error(text: &str) -> Option<String> {
    let line: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if line.is_empty() {
        return None;
    }
    let mut end = line.len().min(ERROR_TEXT_MAX);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(line[..end].to_owned())
}

/// Fold rollout `event_msg` records, oldest first, into turn facts. A new
/// `task_started` clears an earlier error; an `error` stays attached to the turn
/// it ended even when a `task_complete` follows it.
pub fn facts_from_events<'a>(events: impl IntoIterator<Item = &'a Value>) -> TurnFacts {
    let mut f = TurnFacts::default();
    for value in events {
        if value["type"] != "event_msg" {
            continue;
        }
        match value["payload"]["type"].as_str() {
            Some("task_started") => {
                f.started = true;
                f.in_progress = true;
                f.last_status = None;
                f.error = None;
                f.observed = true;
            }
            Some("error") => {
                f.error = value["payload"]["message"].as_str().and_then(clean_error);
                f.observed = true;
            }
            Some("task_complete") => {
                f.started = true;
                f.in_progress = false;
                f.last_status = Some(
                    if f.error.is_some() {
                        "failed"
                    } else {
                        "completed"
                    }
                    .into(),
                );
                f.observed = true;
            }
            Some("turn_aborted") => {
                f.started = true;
                f.in_progress = false;
                f.last_status = Some(
                    if f.error.is_some() {
                        "failed"
                    } else {
                        "interrupted"
                    }
                    .into(),
                );
                f.observed = true;
            }
            _ => {}
        }
    }
    if f.observed {
        f.source = Some("rollout".into());
    }
    f
}

/// Overlay the provider's own thread status, which is newer than any rollout tail.
/// `last_turn` is the latest finished turn summary when the provider was asked
/// (only for a `systemError` thread).
pub fn overlay_thread(f: &mut TurnFacts, status: &Value, last_turn: Option<&Value>) {
    let Some(kind) = status["type"].as_str() else {
        return;
    };
    let flagged = status["activeFlags"]
        .as_array()
        .is_some_and(|flags| !flags.is_empty());
    match kind {
        "active" => {
            f.in_progress = true;
            f.started = true;
            f.blocked = flagged;
            f.error = None;
        }
        "idle" | "systemError" => {
            f.in_progress = false;
            f.blocked = flagged;
        }
        _ => return,
    }
    if let Some(turn) = last_turn {
        f.started = true;
        f.last_status = turn["status"].as_str().map(str::to_owned);
        if let Some(message) = turn["error"]["message"].as_str().and_then(clean_error) {
            f.error = Some(message);
        }
        if kind == "systemError" && f.last_status.as_deref() == Some("completed") {
            f.last_status = Some("failed".into());
        }
    }
    f.observed = true;
    f.source = Some("app_server".into());
}

/// Facts from a native readiness observation (`api observe`, send gate): the
/// model-level state, whether a previous turn exists and the latest turn's outcome.
pub fn facts_from_readiness(
    state: &str,
    blocked: bool,
    has_previous_turn: bool,
    last_status: Option<&str>,
    error: Option<&str>,
) -> TurnFacts {
    if !matches!(state, "idle" | "working" | "interrupted" | "errored") {
        return TurnFacts::default();
    }
    TurnFacts {
        observed: true,
        source: Some("app_server".into()),
        started: has_previous_turn || state != "idle",
        in_progress: state == "working",
        blocked,
        last_status: match state {
            "errored" => Some("failed".into()),
            "interrupted" => Some("interrupted".into()),
            _ => last_status.map(str::to_owned),
        },
        error: error.and_then(clean_error),
    }
}

/// `working`, `idle`, `turn_ended`, `blocked`, `error` or `unknown`.
///
/// `idle`: ready at the prompt and no turn has been seen; `turn_ended`: the latest
/// turn finished (completed or interrupted) and nothing has started since;
/// `error`: the latest turn failed; `blocked`: waiting on an approval or input.
pub fn classify(f: &TurnFacts) -> &'static str {
    if !f.observed {
        "unknown"
    } else if f.blocked {
        "blocked"
    } else if f.in_progress {
        "working"
    } else if f.last_status.as_deref() == Some("failed") || f.error.is_some() {
        "error"
    } else if f.started {
        "turn_ended"
    } else {
        "idle"
    }
}

/// Public `live_activity` row field.
pub fn public(f: Option<&TurnFacts>, observed_at: i64) -> Value {
    let Some(f) = f else {
        return Value::Null;
    };
    let state = classify(f);
    json!({"state":state,"error":if state=="error" {json!(f.error)} else {Value::Null},
        "last_turn_status":f.last_status,"source":f.source,"observed_at":observed_at})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, message: Option<&str>) -> Value {
        json!({"type":"event_msg","payload":{"type":kind,"message":message}})
    }

    #[test]
    fn rollout_events_classify_every_state() {
        let state = |events: &[Value]| classify(&facts_from_events(events));
        assert_eq!(state(&[]), "unknown");
        assert_eq!(state(&[ev("task_started", None)]), "working");
        assert_eq!(
            state(&[ev("task_started", None), ev("task_complete", None)]),
            "turn_ended"
        );
        assert_eq!(
            state(&[ev("task_started", None), ev("turn_aborted", None)]),
            "turn_ended"
        );
        let failed = [
            ev("task_started", None),
            ev(
                "error",
                Some("Selected model is at capacity.\nPlease try again"),
            ),
            ev("task_complete", None),
        ];
        let facts = facts_from_events(&failed);
        assert_eq!(classify(&facts), "error");
        assert_eq!(
            facts.error.as_deref(),
            Some("Selected model is at capacity. Please try again")
        );
        // The next turn clears the error.
        let mut again = failed.to_vec();
        again.push(ev("task_started", None));
        assert_eq!(state(&again), "working");
    }

    #[test]
    fn provider_status_overrides_rollout_and_blocks() {
        let mut f = facts_from_events(&[ev("task_started", None)]);
        overlay_thread(&mut f, &json!({"type":"idle"}), None);
        assert_eq!(classify(&f), "turn_ended");
        overlay_thread(
            &mut f,
            &json!({"type":"active","activeFlags":["waitingOnApproval"]}),
            None,
        );
        assert_eq!(classify(&f), "blocked");
        let mut g = TurnFacts::default();
        overlay_thread(
            &mut g,
            &json!({"type":"systemError"}),
            Some(&json!({"status":"failed","error":{"message":"Selected model is at capacity"}})),
        );
        assert_eq!(classify(&g), "error");
        assert_eq!(
            public(Some(&g), 5)["error"],
            "Selected model is at capacity"
        );
        let mut fresh = TurnFacts::default();
        overlay_thread(&mut fresh, &json!({"type":"idle"}), None);
        assert_eq!(classify(&fresh), "idle");
    }

    #[test]
    fn error_text_is_one_bounded_printable_line() {
        let long = "é".repeat(400);
        assert!(clean_error(&long).unwrap().len() <= ERROR_TEXT_MAX);
        assert_eq!(clean_error("  \n "), None);
    }
}
