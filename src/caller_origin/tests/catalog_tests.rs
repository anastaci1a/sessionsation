use super::super::*;
use crate::{catalog::Attachment, provider::ProviderId};

fn register(paths: &Paths, pid: u32, start: u64) {
    catalog::record_provider_launch(
        paths,
        ProviderId::Claude,
        "personal",
        &uuid::Uuid::new_v4().to_string(),
        "managed",
        None,
        Some(Attachment {
            host: "herdr/x".into(),
            target: "herdr/x/w:p".into(),
            pid,
            process_start: start,
        }),
    )
    .unwrap();
}

#[test]
fn a_caller_inside_a_managed_session_is_an_agent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
    let me = std::process::id();
    let (_, start) = proc_stat(me).unwrap();
    // No attachments: a plain shell.
    assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
    // An unrelated live process is not an ancestor.
    register(&paths, 1, 1);
    assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
    // A recycled pid (same pid, other start time) does not count.
    register(&paths, me, start + 1);
    assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
    // This process (and so everything it runs) is inside the managed session.
    register(&paths, me, start);
    assert_eq!(resolve(&paths, None), ("agent".into(), "detected"));
    // An explicit origin always wins.
    assert_eq!(
        resolve(&paths, Some("operator")),
        ("operator".into(), "explicit")
    );
    assert_eq!(resolve(&paths, Some("agent")), ("agent".into(), "explicit"));
}

#[test]
fn a_managed_session_caller_ignores_the_component() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
    let me = std::process::id();
    let (_, start) = proc_stat(me).unwrap();
    // Outside any session: the bare component names the sender.
    assert_eq!(managed_sender(&paths), None);
    let c = sender_for(&paths, Some("guard"), None);
    assert_eq!(
        (
            c.sender.display_name(),
            c.sender.mode.as_str(),
            c.ignored,
            c.prefixed
        ),
        ("guard", "prompting", None, None)
    );
    // Inside a managed session: its own name, never the component (bare or prefixed).
    register(&paths, me, start);
    let session = managed_sender(&paths).expect("this process is the attachment");
    assert!(!session.name.starts_with("sessionsation"));
    let c = sender_for(&paths, Some("guard"), Some("env-guard"));
    assert_eq!(
        (c.sender, c.ignored, c.prefixed),
        (session.clone(), Some(COMPONENT_IGNORED_SESSION), None)
    );
    // Even a component equal to its own name: still the session, reported as ignored.
    let own = session.name.to_uppercase();
    let c = sender_for(&paths, Some(&own), None);
    assert_eq!(
        (c.sender, c.ignored, c.prefixed),
        (session.clone(), Some(COMPONENT_IGNORED_SESSION), None)
    );
    // The env var alone is ignored too, and not reported as an ignored request field.
    let c = sender_for(&paths, None, Some("env-guard"));
    assert_eq!((c.sender, c.ignored, c.prefixed), (session, None, None));
}

#[test]
fn a_component_naming_a_catalog_session_falls_back_to_the_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
    // Not this process: an outside caller, and a session in another Space.
    let other = catalog::record_provider_launch(
        &paths,
        ProviderId::Claude,
        "personal",
        &uuid::Uuid::new_v4().to_string(),
        "Watcher",
        None,
        None,
    )
    .unwrap();
    catalog::scope::set_space(&paths, &other.id, &other.generation(), Some("ops")).unwrap();
    assert_eq!(
        catalog::list(&paths).unwrap()[0].space.as_deref(),
        Some("ops")
    );
    assert_eq!(managed_sender(&paths), None);
    for spoof in ["watcher", "WATCHER", "Watcher"] {
        let c = sender_for(&paths, Some(spoof), None);
        assert_eq!(
            (c.sender.display_name(), c.ignored, c.prefixed),
            (
                format!("sessionsation:{spoof}").as_str(),
                None,
                Some(COMPONENT_PREFIXED_COLLISION)
            )
        );
    }
    // A unique component stays bare.
    let c = sender_for(&paths, Some("watcher-2"), None);
    assert_eq!((c.sender.display_name(), c.prefixed), ("watcher-2", None));
}
