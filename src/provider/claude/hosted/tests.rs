use super::*;
use crate::config::LaunchPermissions;
fn host(origin: &str, mode: Option<&str>) -> Host {
    Host {
        schema_version: 1,
        conversation_id: "11111111-1111-4111-8111-111111111111".into(),
        session_id: "s".into(),
        account: "a".into(),
        directory: "/".into(),
        origin: origin.into(),
        permission_mode: "account".into(),
        effective_permission_mode: mode.map(str::to_owned),
        epoch: 1,
        state: "starting".into(),
        reason: None,
        worker_pid: None,
        worker_start: None,
        child_pid: None,
        child_start: None,
        busy: false,
        pending_permissions: vec![],
        created_at: 0,
        updated_at: 0,
        started_at: None,
        stopped_at: None,
    }
}
#[test]
fn worker_argv_is_stream_json_with_a_prompt_host_and_never_implicit_bypass() {
    let a = worker_args(&host("new", None));
    for flag in [
        "--input-format",
        "--output-format",
        "--replay-user-messages",
        "--verbose",
    ] {
        assert!(a.iter().any(|x| x == flag), "{a:?}");
    }
    assert!(
        a.windows(2)
            .any(|w| w[0] == "--permission-prompt-tool" && w[1] == "stdio")
    );
    assert!(a.windows(2).any(|w| w[0] == "--session-id"));
    assert!(
        !a.iter()
            .any(|x| x == "--permission-mode" || x.contains("bypass") || x.contains("dangerously"))
    );
    let r = worker_args(&host("resume", Some("plan")));
    assert!(r.windows(2).any(|w| w[0] == "--resume"));
    assert!(
        r.windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "plan")
    );
    let d = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_mode(None, d.path(), LaunchPermissions::Default).unwrap(),
        ("account".into(), None)
    );
    fs::write(
        d.path().join("settings.json"),
        r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
    )
    .unwrap();
    assert_eq!(
        resolve_mode(None, d.path(), LaunchPermissions::Default).unwrap(),
        ("account".into(), Some("default".into())),
        "an account bypass default is never inherited silently"
    );
    assert_eq!(
        resolve_mode(
            Some("bypassPermissions"),
            d.path(),
            LaunchPermissions::Default
        )
        .unwrap()
        .1
        .as_deref(),
        Some("bypassPermissions")
    );
    assert!(resolve_mode(Some("yolo"), d.path(), LaunchPermissions::Default).is_err());
    // The operator's launch default (yolo) bypasses unless a mode is named.
    assert_eq!(
        resolve_mode(None, d.path(), LaunchPermissions::Yolo).unwrap(),
        ("bypassPermissions".into(), Some("bypassPermissions".into()))
    );
    assert_eq!(
        resolve_mode(Some("plan"), d.path(), LaunchPermissions::Yolo).unwrap(),
        ("plan".into(), Some("plan".into()))
    );
    assert_eq!(
        resolve_mode(Some("account"), d.path(), LaunchPermissions::Yolo).unwrap(),
        ("account".into(), Some("default".into()))
    );
}
#[test]
fn rendering_shows_prompts_and_replies_but_not_lifecycle_noise() {
    assert_eq!(
        render(&json!({"kind":"assistant","text":"hi"})).unwrap(),
        "hi"
    );
    assert!(render(&json!({"kind":"lifecycle"})).is_none());
    let p = render(&json!({"kind":"permission_request","tool_name":"Bash","input_preview":"{}","session_id":"S","request_id":"R"})).unwrap();
    assert!(p.contains("--request-id R") && p.contains("pending"));
}
