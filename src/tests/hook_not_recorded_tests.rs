use super::super::*;

#[test]
fn only_the_not_yet_recorded_state_unverified_is_retryable() {
    let fresh = anyhow::Error::new(sessionsation::messaging::model_change::claude::HookNotRecorded);
    let e = error_envelope(&fresh);
    assert_eq!(e["error"]["code"], "SESSION_STATE_UNVERIFIED");
    assert_eq!(e["error"]["retryable"], true);
    assert_eq!(e["error"]["detail"], "hook_not_recorded");
    let other = anyhow::anyhow!("SESSION_STATE_UNVERIFIED: the session reports no model");
    let e = error_envelope(&other);
    assert_eq!(e["error"]["code"], "SESSION_STATE_UNVERIFIED");
    assert_eq!(e["error"]["retryable"], false);
    assert!(e["error"]["detail"].is_null());
}
