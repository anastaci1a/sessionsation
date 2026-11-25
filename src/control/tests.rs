use super::*;

#[test]
fn failed_system_error_is_quiescent_only_with_terminal_turn_evidence() {
    let mut thread = json!({"status":{"type":"systemError"},"turns":[
        {"status":"completed"},
        {"status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}
    ]});
    assert!(confirmed_quiescent(&thread, false));
    thread["turns"][0]["status"] = json!("inProgress");
    assert!(!confirmed_quiescent(&thread, false));
    thread["turns"][0]["status"] = json!("completed");
    thread["turns"][1]["status"] = json!("inProgress");
    assert!(!confirmed_quiescent(&thread, false));
    thread["turns"] = json!([]);
    assert!(!confirmed_quiescent(&thread, false));
    thread["status"]["type"] = json!("active");
    assert!(!confirmed_quiescent(&thread, false));
    for status in ["completed", "interrupted", "failed"] {
        thread = json!({"status":{"type":"systemError"},"turns":[{"status":status}]});
        assert!(confirmed_quiescent(&thread, false));
        thread["status"]["activeFlags"] = json!(["waitingOnApproval"]);
        assert!(!confirmed_quiescent(&thread, false));
    }
}

#[test]
fn verified_permissions_persist_as_native_thread_settings() {
    let full = json!({"thread":{"id":"t1"},"approvalPolicy":"never",
        "sandbox":{"type":"dangerFullAccess"},"activePermissionProfile":{"id":":danger-full-access"}});
    assert_eq!(
        settings_update(&full, None).unwrap(),
        json!({"threadId":"t1","approvalPolicy":"never","permissions":":danger-full-access"})
    );
    let restricted = json!({"thread":{"id":"t1"},"approvalPolicy":"on-request",
        "sandbox":{"type":"workspaceWrite","networkAccess":false},"activePermissionProfile":null});
    assert_eq!(
        settings_update(&restricted, Some(json!("auto_review"))).unwrap(),
        json!({"threadId":"t1","approvalPolicy":"on-request",
            "sandboxPolicy":{"type":"workspaceWrite","networkAccess":false},
            "approvalsReviewer":"auto_review"})
    );
    assert!(settings_update(&json!({"approvalPolicy":"never"}), None).is_err());
}

#[test]
fn full_access_requires_matching_sandbox_approval_and_named_profile() {
    let correct = json!({"approvalPolicy":"never","sandbox":{"type":"dangerFullAccess"},
        "activePermissionProfile":{"id":":danger-full-access"}});
    assert!(verify_permissions(&correct, Some("never"), Some("danger-full-access")).is_ok());
    for field in [
        "/approvalPolicy",
        "/sandbox/type",
        "/activePermissionProfile/id",
    ] {
        let mut wrong = correct.clone();
        *wrong.pointer_mut(field).unwrap() = Value::Null;
        assert!(verify_permissions(&wrong, Some("never"), Some("danger-full-access")).is_err());
    }
}
