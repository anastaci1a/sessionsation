use super::super::*;
#[test]
fn only_verified_pre_effect_codes_release_uncertain_move_intent() {
    for code in ["pane_not_found", "tab_not_found", "workspace_not_found"] {
        assert!(move_rejected_before_effect(
            &HostRejection(code.into()).into()
        ));
    }
    for code in [
        "pane_move_failed",
        "pane_layout_unavailable",
        "timeout",
        "unknown",
    ] {
        assert!(!move_rejected_before_effect(
            &HostRejection(code.into()).into()
        ));
    }
    assert!(!move_rejected_before_effect(&anyhow::anyhow!(
        "workspace_not_found"
    )));
}
