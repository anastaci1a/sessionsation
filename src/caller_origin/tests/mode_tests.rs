use super::super::*;

fn session(provider: &str) -> catalog::Session {
    catalog::Session {
        id: "i".into(),
        name: "n".into(),
        provider: provider.into(),
        account: "a".into(),
        conversation_id: "c".into(),
        revision: 1,
        attachment: Some(catalog::Attachment {
            host: "h".into(),
            target: "t".into(),
            pid: 9,
            process_start: 1,
        }),
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    }
}
fn args(v: &[&str]) -> Option<Vec<String>> {
    Some(v.iter().map(|s| s.to_string()).collect())
}

#[test]
fn claude_mode_follows_its_own_launch_flags() {
    let s = session("claude");
    let mode = |v: &'static [&'static str]| session_mode(&s, move |_| args(v), |_| None);
    assert_eq!(
        mode(&["claude", "--dangerously-skip-permissions"]),
        "bypass"
    );
    assert_eq!(
        mode(&["claude", "--permission-mode", "bypassPermissions"]),
        "bypass"
    );
    assert_eq!(
        mode(&["claude", "--permission-mode=bypassPermissions"]),
        "bypass"
    );
    assert_eq!(mode(&["claude", "--permission-mode", "plan"]), "prompting");
    assert_eq!(mode(&["claude"]), "prompting");
    assert_eq!(session_mode(&s, |_| None, |_| None), "prompting");
}

#[test]
fn codex_mode_needs_never_and_full_access() {
    let s = session("codex");
    let mode = |a: &str, sb: &str| {
        let p = Some((a.to_owned(), sb.to_owned()));
        session_mode(&s, |_| None, move |_| p.clone())
    };
    assert_eq!(mode("never", "danger-full-access"), "bypass");
    assert_eq!(mode("never", "workspace-write"), "prompting");
    assert_eq!(mode("on-request", "danger-full-access"), "prompting");
    assert_eq!(session_mode(&s, |_| None, |_| None), "prompting");
    // Another provider's flags never count.
    assert_eq!(
        session_mode(
            &session("gemini"),
            |_| args(&["--dangerously-skip-permissions"]),
            |_| None
        ),
        "prompting"
    );
}
