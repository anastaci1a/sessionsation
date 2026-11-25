use super::super::*;
use crate::{catalog::Attachment, provider::ProviderId};

fn table(pid: u32) -> Option<(u32, u64)> {
    match pid {
        50 => Some((40, 500)),
        40 => Some((30, 400)),
        30 => Some((1, 300)),
        _ => None,
    }
}

fn sessions(paths: &Paths) -> Vec<catalog::Session> {
    let att = |pid| Attachment {
        host: "h".into(),
        target: "h/t".into(),
        pid,
        process_start: 300,
    };
    let launch = |p, conv: &str, pid| {
        catalog::record_provider_launch(paths, p, "a", conv, "managed", None, Some(att(pid)))
            .unwrap();
    };
    launch(ProviderId::Claude, "claude-conv", 30);
    launch(ProviderId::Codex, "thread-1", 7777);
    catalog::list(paths).unwrap()
}

fn name_of(s: Option<&catalog::Session>) -> Option<String> {
    s.map(|s| s.name.clone())
}

#[test]
fn ancestry_hit_wins_over_codex_thread() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
    let ss = sessions(&paths);
    let claude = ss.iter().find(|s| s.provider == "claude").unwrap();
    let hit = find_session(&ss, 50, table, Some("thread-1".into()), |_| {
        Some("thread-1".into())
    });
    assert_eq!(name_of(hit), Some(claude.name.clone()));
}

#[test]
fn codex_thread_via_own_env_or_ancestor_after_env_i() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
    let ss = sessions(&paths);
    let codex = ss
        .iter()
        .find(|s| crate::provider::ops::supervised(&s.provider))
        .unwrap();
    let chain = |p: u32| if p == 50 { Some((40, 1)) } else { Some((1, 1)) };
    // Own env.
    let hit = find_session(&ss, 50, chain, Some("thread-1".into()), |_| None);
    assert_eq!(name_of(hit), Some(codex.name.clone()));
    // env -i: own env empty, the thread id lives on an ancestor.
    let hit = find_session(&ss, 50, chain, None, |p| {
        (p == 40).then(|| "thread-1".to_owned())
    });
    assert_eq!(name_of(hit), Some(codex.name.clone()));
    // A thread id equal to a non-codex conversation id does not match.
    assert!(find_session(&ss, 50, chain, None, |_| Some("claude-conv".into())).is_none());
}

#[test]
fn unknown_thread_is_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
    let ss = sessions(&paths);
    let chain = |_: u32| Some((1, 1));
    assert!(find_session(&ss, 50, chain, Some("nope".into()), |_| None).is_none());
    assert!(find_session(&ss, 50, chain, None, |_| Some("nope".into())).is_none());
    assert!(find_session(&ss, 50, chain, None, |_| None).is_none());
    assert!(chain_thread(5, |p| Some((p % 2 + 2, 1)), |_| None).is_none());
}

#[test]
fn environ_parsing_returns_only_the_thread_id() {
    let raw = b"A=1\0CODEX_THREAD_ID=abc-123\0CODEX_THREAD_ID_X=no\0SECRET=s\0";
    assert_eq!(thread_from_environ(raw), Some("abc-123".into()));
    assert_eq!(thread_from_environ(b"A=1\0CODEX_THREAD_ID=\0"), None);
    assert_eq!(thread_from_environ(b"A=1\0"), None);
}
