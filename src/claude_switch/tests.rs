use super::*;
use std::cell::RefCell;
use std::os::unix::process::CommandExt;

thread_local! {
    static CRASH_AT: RefCell<Option<&'static str>> = const { RefCell::new(None) };
}
/// Simulated crash: the journal keeps the phase just written, nothing after runs.
pub(super) fn crash_check(name: &str) -> Result<()> {
    ensure!(
        CRASH_AT.with(|c| *c.borrow()) != Some(name),
        "SIMULATED_CRASH at {name}"
    );
    Ok(())
}
fn crash_at(phase: Option<&'static str>) {
    CRASH_AT.with(|c| *c.borrow_mut() = phase);
}

struct Fixture {
    _dir: tempfile::TempDir,
    paths: Paths,
    a: PathBuf,
    b: PathBuf,
    conversation: String,
    other: String,
    project: PathBuf,
    record: Record,
}
const SLUG: &str = "-work-project";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn tree(home: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push((
                    p.strip_prefix(base).unwrap().to_path_buf(),
                    fs::read(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = vec![];
    walk(home, home, &mut out);
    out.sort();
    out
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let (a, b) = (dir.path().join("home-a"), dir.path().join("home-b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let conversation = uuid::Uuid::new_v4().to_string();
    let other = uuid::Uuid::new_v4().to_string();
    let p = format!("projects/{SLUG}");
    write(
        &a.join(format!("{p}/{conversation}.jsonl")),
        "{\"type\":\"user\"}\n",
    );
    write(
        &a.join(format!("{p}/{conversation}/tool-results/r.txt")),
        "result",
    );
    write(
        &a.join(format!("file-history/{conversation}/h1")),
        "history",
    );
    write(&a.join(format!("session-env/{conversation}/e1")), "env");
    write(&a.join(format!("todos/{conversation}-agent-1.json")), "[]");
    write(&a.join(format!("{p}/memory/MEMORY.md")), "account memory");
    write(
        &a.join(format!("{p}/{other}.jsonl")),
        "{\"type\":\"user\"}\n",
    );
    write(&a.join("settings.json"), "{}");
    let s = catalog::record_provider_launch(
        &paths,
        ProviderId::Claude,
        "a",
        &conversation,
        "label",
        project.to_str(),
        None,
    )
    .unwrap();
    let record = Record {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        kind: "account".into(),
        session_id: s.id.clone(),
        conversation_id: conversation.clone(),
        conversation_ids: vec![conversation.clone()],
        expected_generation: s.generation(),
        accept_risks: false,
        source_account: "a".into(),
        target_account: "b".into(),
        source_home: a.clone(),
        target_home: b.clone(),
        directory: project.to_str().map(str::to_owned),
        pane: None,
        process: None,
        permissions: None,
        entries: None,
        phase: "admitted".into(),
        detail: String::new(),
        uncertain_from: None,
        resume: "pending".into(),
        generation_after: None,
        created_at: daemon::now(),
        updated_at: daemon::now(),
        session: s,
    };
    let mut f = Fixture {
        _dir: dir,
        paths,
        a,
        b,
        conversation,
        other,
        project,
        record,
    };
    save(&f.paths, &mut f.record).unwrap();
    f
}

impl Fixture {
    fn moved_paths(&self) -> Vec<String> {
        let c = &self.conversation;
        vec![
            format!("projects/{SLUG}/{c}.jsonl"),
            format!("projects/{SLUG}/{c}/tool-results/r.txt"),
            format!("file-history/{c}/h1"),
            format!("session-env/{c}/e1"),
            format!("todos/{c}-agent-1.json"),
        ]
    }
    fn stays_paths(&self) -> Vec<String> {
        vec![
            format!("projects/{SLUG}/memory/MEMORY.md"),
            format!("projects/{SLUG}/{}.jsonl", self.other),
            "settings.json".into(),
        ]
    }
    fn assert_switched(&self) {
        let r = read(&self.paths, &self.record.id).unwrap();
        assert_eq!(r.phase, "complete", "{}", r.detail);
        for p in self.moved_paths() {
            assert!(self.b.join(&p).exists(), "{p} missing in B");
            assert!(!self.a.join(&p).exists(), "{p} still in A");
        }
        assert_eq!(
            fs::read(self.b.join(&self.moved_paths()[0])).unwrap(),
            b"{\"type\":\"user\"}\n"
        );
        // Account state stays with A, byte for byte, and B gains none of it.
        assert_eq!(
            fs::read(self.a.join(format!("projects/{SLUG}/memory/MEMORY.md"))).unwrap(),
            b"account memory"
        );
        for p in self.stays_paths() {
            assert!(self.a.join(&p).exists(), "{p} must stay in A");
            assert!(!self.b.join(&p).exists(), "{p} must not reach B");
        }
        assert!(!self.b.join(STAGING).exists(), "staging left behind");
        let s = catalog::find(&self.paths, &self.record.session_id).unwrap();
        assert_eq!(s.account, "b");
        assert!(s.attachment.is_none());
        assert_eq!(r.generation_after, Some(s.generation()));
        assert_ne!(s.generation(), self.record.expected_generation);
        assert_eq!(r.resume, "not_needed");
    }
}

#[tokio::test]
async fn idle_session_moves_its_exact_file_set_and_leaves_account_memory() {
    let mut f = fixture();
    let before_a = tree(&f.a);
    run(&f.paths, &mut f.record).await;
    f.assert_switched();
    // Exactly the stays-paths remain in A: nothing else was touched.
    let after_a: Vec<_> = tree(&f.a).into_iter().map(|(p, _)| p).collect();
    let expected: Vec<_> = before_a
        .iter()
        .filter(|(p, _)| f.stays_paths().iter().any(|s| p == Path::new(s)))
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(after_a, expected);
    let public = public(&f.paths, &read(&f.paths, &f.record.id).unwrap());
    assert_eq!(public["receipt"]["account_memory"]["moved"], false);
    assert_eq!(
        public["receipt"]["account_memory"]["stays_with"],
        "claude:a"
    );
    assert_eq!(public["receipt"]["files_moved"], 5);
    assert_eq!(public["provider"], "claude");
    assert_eq!(public["kind"], "account");
}

#[tokio::test]
async fn a_crash_at_every_phase_recovers_to_the_same_final_state() {
    for phase in [
        "stopping",
        "stopped",
        "copying",
        "copied",
        "switching",
        "switched",
        "removing",
        "removed",
    ] {
        let mut f = fixture();
        crash_at(Some(phase));
        let error = drive(&f.paths, &mut f.record).await.unwrap_err();
        assert!(format!("{error:#}").contains("SIMULATED_CRASH"), "{phase}");
        crash_at(None);
        let journal = read(&f.paths, &f.record.id).unwrap();
        assert_eq!(journal.phase, phase, "the journal keeps the crash phase");
        assert!(!is_terminal(&journal));
        if order(phase) >= order("switched") {
            // Past the commit point the catalog already names the target.
            assert_eq!(
                catalog::find(&f.paths, &f.record.session_id)
                    .unwrap()
                    .account,
                "b"
            );
        }
        if phase == "copying" {
            // Interrupted staging is discarded and redone, never trusted.
            let stage = f.b.join(STAGING).join(&f.record.id).join("0");
            write(&stage.join("partial"), "half");
        }
        let value = recover(&f.paths, &f.record.id).await.unwrap();
        assert_eq!(value["phase"], "complete", "{phase}: {value}");
        f.assert_switched();
        // Recovering a finished operation is a harmless no-op.
        assert_eq!(
            recover(&f.paths, &f.record.id).await.unwrap()["phase"],
            "complete"
        );
    }
}

#[tokio::test]
async fn failure_before_the_catalog_changes_rolls_back_and_keeps_a_foreign_target_file() {
    let mut f = fixture();
    let before = tree(&f.a);
    // A different file already sits at the target path: never overwritten, never removed.
    let theirs = f.b.join(format!("todos/{}-agent-1.json", f.conversation));
    write(&theirs, "someone else's");
    run(&f.paths, &mut f.record).await;
    let r = read(&f.paths, &f.record.id).unwrap();
    assert_eq!(r.phase, "failed", "{}", r.detail);
    assert!(r.detail.contains("TARGET_CONFLICT"), "{}", r.detail);
    assert_eq!(tree(&f.a), before, "source untouched");
    assert_eq!(fs::read(&theirs).unwrap(), b"someone else's");
    assert_eq!(tree(&f.b).len(), 1, "only the foreign file is in B");
    assert_eq!(
        catalog::find(&f.paths, &f.record.session_id)
            .unwrap()
            .account,
        "a"
    );
    assert_eq!(
        catalog::find(&f.paths, &f.record.session_id)
            .unwrap()
            .generation(),
        f.record.expected_generation
    );
}

#[tokio::test]
async fn failure_after_the_catalog_changes_is_uncertain_and_recoverable_then_abandonable() {
    let mut f = fixture();
    // The source copy changes after the catalog switch: removal refuses to delete it.
    crash_at(Some("removing"));
    drive(&f.paths, &mut f.record).await.unwrap_err();
    crash_at(None);
    let tampered = f.a.join(&f.moved_paths()[0]);
    fs::write(&tampered, "changed after verification").unwrap();
    let value = recover(&f.paths, &f.record.id).await.unwrap();
    assert_eq!(value["phase"], "uncertain", "{value}");
    assert!(
        value["detail"]
            .as_str()
            .unwrap()
            .contains("HISTORY_CHANGED")
    );
    assert_eq!(fs::read(&tampered).unwrap(), b"changed after verification");
    assert_eq!(value["can_recover"], true);
    // Abandon needs the exact confirmation and terminates without touching files.
    assert!(abandon(&f.paths, &f.record.id, "nope").is_err());
    let abandoned = abandon(&f.paths, &f.record.id, &f.record.id).unwrap();
    assert_eq!(abandoned["phase"], "abandoned");
    assert_eq!(fs::read(&tampered).unwrap(), b"changed after verification");
    assert!(abandon(&f.paths, &f.record.id, &f.record.id).is_err());
}

#[tokio::test]
async fn abandon_refuses_a_healthy_in_flight_operation() {
    let f = fixture();
    let error = abandon(&f.paths, &f.record.id, &f.record.id).unwrap_err();
    assert!(format!("{error:#}").contains("OPERATION_NOT_STUCK"));
}

#[tokio::test]
async fn a_running_process_is_stopped_by_exact_identity_on_recovery() {
    let mut f = fixture();
    let mut child = std::process::Command::new("sleep")
        .arg("600")
        .env("CLAUDE_CONFIG_DIR", &f.a)
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let wait_start = || loop {
        if let Some((_, start)) = claude::proc_start(pid) {
            return start;
        }
    };
    let start = wait_start();
    while fs::read(format!("/proc/{pid}/environ")).map_or(true, |e| e.is_empty()) {}
    write(
        &f.a.join(format!("sessions/{pid}.json")),
        &json!({"pid":pid,"sessionId":f.conversation,"cwd":f.project,"procStart":start.to_string(),
            "kind":"interactive","status":"idle"})
        .to_string(),
    );
    f.record.process = Some(Process {
        pid,
        process_start: start,
        status: Some("idle".into()),
    });
    save(&f.paths, &mut f.record).unwrap();
    crash_at(Some("stopping"));
    drive(&f.paths, &mut f.record).await.unwrap_err();
    crash_at(None);
    assert!(
        child.try_wait().unwrap().is_none(),
        "nothing was signalled yet"
    );
    let value = recover(&f.paths, &f.record.id).await.unwrap();
    assert_eq!(value["phase"], "complete", "{value}");
    assert!(child.try_wait().unwrap().is_some() || gone(pid, start));
    let _ = child.wait();
    // The stale registry entry of the stopped process left with the source copies.
    assert!(!f.a.join(format!("sessions/{pid}.json")).exists());
    let r = read(&f.paths, &f.record.id).unwrap();
    assert_eq!(r.phase, "complete");
    assert_eq!(r.resume, "not_needed", "no pane was recorded");
}

#[tokio::test]
async fn a_replaced_process_is_never_signalled() {
    let mut f = fixture();
    let child = std::process::Command::new("sleep")
        .arg("600")
        .env("CLAUDE_CONFIG_DIR", &f.a)
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut child = child;
    while claude::proc_start(pid).is_none() {}
    let real = claude::proc_start(pid).unwrap().1;
    while fs::read(format!("/proc/{pid}/environ")).map_or(true, |e| e.is_empty()) {}
    write(
        &f.a.join(format!("sessions/{pid}.json")),
        &json!({"pid":pid,"sessionId":f.conversation,"cwd":f.project,"procStart":real.to_string(),
            "kind":"interactive","status":"idle"})
        .to_string(),
    );
    // The journal recorded a different identity (a reused pid): refuse, signal nothing.
    f.record.process = Some(Process {
        pid,
        process_start: real + 1,
        status: None,
    });
    run(&f.paths, &mut f.record).await;
    let r = read(&f.paths, &f.record.id).unwrap();
    assert_eq!(r.phase, "failed", "{}", r.detail);
    assert!(r.detail.contains("SESSION_CHANGED"), "{}", r.detail);
    assert!(child.try_wait().unwrap().is_none(), "process must be alive");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn the_phase_order_is_total_and_terminal_phases_are_unordered() {
    assert!(order("admitted") < order("copying") && order("switched") < order("removing"));
    assert_eq!(order("failed"), usize::MAX);
}
