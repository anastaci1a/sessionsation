use super::*;
#[test]
fn actions_preserve_modes_and_stop_intent() {
    for state in [
        "idle",
        "working",
        "interrupted",
        "errored",
        "disconnected",
        "unknown",
    ] {
        let old = actions(state, None, false, true);
        assert_eq!(old["resume"]["available"], false);
        assert_eq!(old["submit"]["available"], state == "working");
        let new = actions(state, None, true, true);
        if state == "idle" {
            assert!(new["submit"]["effective_action"].is_null());
        }
        assert_eq!(
            new["resume"]["available"],
            matches!(state, "interrupted" | "errored")
        );
        assert_eq!(
            new["submit"]["available"],
            matches!(state, "working" | "idle")
        );
        assert_eq!(
            new["queue"]["available"],
            matches!(state, "working" | "idle")
        );
        for reason in [None, Some("approval_or_user_input_required")] {
            let disabled = actions(state, reason, true, false);
            for action in ["submit", "queue", "resume"] {
                assert_eq!(disabled[action]["available"], false);
            }
        }
    }
}
