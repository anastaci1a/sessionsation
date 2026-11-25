use super::super::*;
#[test]
fn launched_process_without_registry_or_transcript_is_running_not_stale() {
    let home = tempfile::tempdir().unwrap();
    let id = "44444444-4444-4444-8444-444444444444";
    let mut child = std::process::Command::new("bash")
        .args(["-c", "sleep 30; sleep 1", "_", "--session-id", id])
        .spawn()
        .unwrap();
    let pid = child.id();
    // The child may still be mid-exec; wait until its argv is visible.
    for _ in 0..200 {
        if cmdline(pid).iter().any(|a| a == id) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let (_, process_start) = proc_start(pid).unwrap();
    let a = catalog::Attachment {
        host: "herdr".into(),
        target: "herdr/1:1:1".into(),
        pid,
        process_start,
    };
    let live: LiveProbe = Ok(vec![]);
    let o = observe_in(home.path(), id, Some(&a), &live);
    assert_eq!(o.availability, "running");
    assert_eq!(o.pid, Some(pid));
    assert!(o.transcript.is_none(), "no conversation is fabricated");
    assert!(
        o.detail
            .as_deref()
            .unwrap()
            .starts_with("NO_CONVERSATION_YET")
    );
    // Another conversation id, or no recorded process, stays stale.
    let other = "55555555-5555-4555-8555-555555555555";
    assert_eq!(
        observe_in(home.path(), other, Some(&a), &live).availability,
        "stale"
    );
    assert_eq!(
        observe_in(home.path(), id, None, &live).availability,
        "stale"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        observe_in(home.path(), id, Some(&a), &live).availability,
        "stale"
    );
}
