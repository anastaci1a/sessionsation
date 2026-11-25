use super::*;
fn home() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("sessions")).unwrap();
    t
}
fn files(h: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(h.join("sessions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}
#[test]
fn registers_with_the_expected_shape_and_drop_removes_it() {
    let h = home();
    let g = register(h.path(), "localica-agent-11", Path::new("/tmp/x/1-ab.sock")).unwrap();
    let v: Value = serde_json::from_slice(&fs::read(g.path()).unwrap()).unwrap();
    assert_eq!(v["name"], "localica-agent-11");
    assert_eq!(v["pid"], std::process::id());
    assert_eq!(v["peerProtocol"], 1);
    assert_eq!(v["messagingSocketPath"], "/tmp/x/1-ab.sock");
    assert!(v.get("sessionId").is_none() && v.get("kind").is_none());
    assert!(owned(&v));
    drop(g);
    assert!(files(h.path()).is_empty());
}
#[test]
fn entry_is_gone_after_an_error_path_unwinds() {
    let h = home();
    let r: Result<()> = (|| {
        let _g = register(h.path(), "s", Path::new("/tmp/s.sock"))?;
        anyhow::bail!("delivery failed");
    })();
    assert!(r.is_err());
    assert!(files(h.path()).is_empty());
}
#[test]
fn a_live_name_collision_or_foreign_pid_file_is_not_touched() {
    let h = home();
    let me = std::process::id();
    let (_, start) = proc_start(me).unwrap();
    // A "real" session with the same name that is alive (this test process).
    let real =
        json!({"pid":1u32.max(me),"procStart":start.to_string(),"name":"dup","sessionId":"s"});
    fs::write(h.path().join("sessions/99999999.json"), real.to_string()).unwrap();
    let mut real2 = real.clone();
    real2["pid"] = json!(me);
    fs::write(h.path().join("sessions/other.json"), real2.to_string()).unwrap();
    let err = register(h.path(), "dup", Path::new("/tmp/s.sock"))
        .err()
        .unwrap();
    assert!(err.to_string().contains("COLLIDES"), "{err}");
    // Foreign file under our own pid is never replaced.
    fs::write(
        h.path().join(format!("sessions/{me}.json")),
        real.to_string(),
    )
    .unwrap();
    let err = register(h.path(), "fresh", Path::new("/tmp/s.sock"))
        .err()
        .unwrap();
    assert!(err.to_string().contains("FOREIGN"), "{err}");
    assert_eq!(files(h.path()).len(), 3);
    assert_eq!(sweep(h.path()), 0);
    assert_eq!(files(h.path()).len(), 3);
}
#[test]
fn sweep_removes_only_owned_dead_entries_after_sigkill() {
    let h = home();
    // A child registers itself then is SIGKILLed: no drop runs.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let cpid = child.id();
    let (_, cstart) = proc_start(cpid).unwrap();
    let entry = |pid: u32, start: String, owned_marker: bool| {
        let mut v = json!({"pid":pid,"procStart":start,"name":"n","entrypoint":if owned_marker {ENTRYPOINT} else {"cli"}});
        if owned_marker {
            v[MARKER_FIELD] = json!(MARKER_VALUE);
        }
        v.to_string()
    };
    let sess = h.path().join("sessions");
    fs::write(
        sess.join(format!("{cpid}.json")),
        entry(cpid, cstart.to_string(), true),
    )
    .unwrap();
    // Live owned entry stays; reused pid (procStart mismatch) goes; foreign dead stays.
    let me = std::process::id();
    let (_, mystart) = proc_start(me).unwrap();
    fs::write(
        sess.join(format!("{me}.json")),
        entry(me, mystart.to_string(), true),
    )
    .unwrap();
    fs::write(sess.join("4000000.json"), entry(4000000, "1".into(), false)).unwrap();
    fs::write(sess.join("4000001.json"), entry(4000001, "1".into(), true)).unwrap();
    assert_eq!(sweep(h.path()), 1); // only the dead owned one (4000001)
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(sweep(h.path()), 1); // the SIGKILLed worker's entry
    let mut expected = vec![format!("{me}.json"), "4000000.json".to_owned()];
    expected.sort();
    assert_eq!(files(h.path()), expected);
}
