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
