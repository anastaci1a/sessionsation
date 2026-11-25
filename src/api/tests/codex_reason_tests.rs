use super::super::*;
fn row(caps: Value) -> Value {
    json!({"capabilities":caps,"capability_reasons":{"add_to_herdr":"X"}})
}
#[test]
fn every_false_codex_capability_has_a_reason() {
    let all = [
        "move_space",
        "resume",
        "change_directory",
        "move_project",
        "switch_account",
        "restart",
        "stop",
        "archive",
        "delete",
    ];
    let falses = |v: &Value| {
        let mut v = v.clone();
        for k in all {
            v["capabilities"][k] = json!(false);
        }
        v
    };
    let ctx = |running, unambiguous, known, in_herdr, archived| CodexReasonContext {
        running,
        unambiguous,
        known,
        in_herdr,
        resume_supported: true,
        archived,
    };
    let mut v = falses(&row(json!({})));
    fill_codex_reasons(&mut v, &ctx(false, true, true, false, false));
    for k in all {
        assert!(v["capability_reasons"][k].is_string(), "{k}: {v}");
    }
    assert_eq!(v["capability_reasons"]["stop"], "SESSION_NOT_RUNNING");
    assert_eq!(
        v["capability_reasons"]["add_to_herdr"], "X",
        "existing reasons are kept"
    );
    let mut v = falses(&row(json!({})));
    fill_codex_reasons(&mut v, &ctx(true, false, true, true, false));
    assert_eq!(v["capability_reasons"]["stop"], "AMBIGUOUS_SESSION");
    assert_eq!(
        v["capability_reasons"]["archive"],
        "SESSION_RUNNING_STOP_FIRST"
    );
    assert_eq!(v["capability_reasons"]["resume"], "SESSION_ALREADY_RUNNING");
    let mut v = falses(&row(json!({})));
    fill_codex_reasons(&mut v, &ctx(true, true, true, false, false));
    assert_eq!(v["capability_reasons"]["move_space"], "NOT_IN_HERDR");
    let mut v = falses(&row(json!({})));
    fill_codex_reasons(&mut v, &ctx(true, true, false, true, false));
    assert_eq!(v["capability_reasons"]["switch_account"], "ACCOUNT_UNKNOWN");
    let mut v = falses(&row(json!({})));
    fill_codex_reasons(&mut v, &ctx(false, true, true, false, true));
    assert_eq!(v["capability_reasons"]["restart"], "SESSION_ARCHIVED");
    // True capabilities get no reason.
    let mut v = row(json!({"stop":true}));
    fill_codex_reasons(&mut v, &ctx(true, true, true, true, false));
    assert!(v["capability_reasons"]["stop"].is_null());
}
