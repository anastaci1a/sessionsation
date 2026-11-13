//! `launch-run` against a held account operation lock (contract 1.49.1): the launch
//! waits for the lock, records why, and a refusal before any provider mutation is a
//! certain `failed` (name released) with a one-line reason on the pane's stderr.
//! Disposable data roots only; no daemon, provider, network or credentials.
use sessionsation::{catalog, launch, paths::Paths, provider::ProviderId};
use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
/// Kills only the recorded launch-run child if a test fails before it exits.
struct Process(Option<Child>);
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGKILL);
                }
            }
            let _ = child.wait();
        }
    }
}

/// A Codex launch receipt in `dispatching` with its name reserved, as the worker leaves it.
fn dispatched(paths: &Paths, name: &str) -> String {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../docs/contract/fixtures.json")).unwrap();
    let mut r = fixtures["launch_receipt"].clone();
    let id = uuid::Uuid::new_v4().to_string();
    r["request"]["request_id"] = id.clone().into();
    r["request"]["name"] = name.into();
    r["phase"] = "dispatching".into();
    let dir = paths.root.join(sessionsation::layout::LAUNCHES);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), r.to_string()).unwrap();
    catalog::reserve_name(paths, None, name, &id).unwrap();
    id
}

fn spawn(paths: &Paths, id: &str, wait_ms: u64) -> Process {
    Process(Some(
        Command::new(binary())
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["launch-run", id])
            .env("SESSIONSATION_HERDR_SESSION", "fixture")
            .env("SESSIONSATION_ACCOUNT_WAIT_MS", wait_ms.to_string())
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_ENV")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ))
}

fn finish(mut p: Process) -> (i32, String) {
    let until = Instant::now() + Duration::from_secs(30);
    while p.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < until, "launch-run did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = p.0.take().unwrap().wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn assert_failed_and_released(paths: &Paths, id: &str, name: &str, code: &str) {
    let r = launch::status(paths, id).unwrap();
    assert_eq!(r.phase, "failed", "{r:?}");
    assert!(r.detail.as_deref().unwrap().starts_with(code), "{r:?}");
    assert!(r.session_id.is_none());
    catalog::reserve_name(paths, None, name, "someone-else").unwrap();
}

#[test]
fn launch_run_held_past_the_window_fails_certainly_and_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let id = dispatched(&paths, "busy-name");
    let _archive =
        sessionsation::accounts::operation_lock(&paths, ProviderId::Codex, "fixture").unwrap();
    let start = Instant::now();
    let (code, stderr) = finish(spawn(&paths, &id, 400));
    assert!(start.elapsed() >= Duration::from_millis(400));
    assert_eq!(code, 1, "{stderr}");
    let line =
        "sessionsation launch failed: ACCOUNT_BUSY: an account operation held the lock for 400ms";
    assert_eq!(stderr.trim_end(), line, "{stderr}");
    assert_failed_and_released(&paths, &id, "busy-name", "ACCOUNT_BUSY");
}

#[test]
fn launch_run_waits_for_the_operation_then_a_pre_start_refusal_is_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let id = dispatched(&paths, "waits-name");
    // The next pre-start check after the lock refuses (account removed): proves the launch
    // got past the lock once the operation ended, without starting any daemon or provider.
    let markers = paths
        .root
        .join(sessionsation::layout::ACCOUNT_CATALOG)
        .join("codex");
    std::fs::create_dir_all(&markers).unwrap();
    std::fs::write(markers.join("fixture.removed.json"), "{}").unwrap();
    let archive =
        sessionsation::accounts::operation_lock(&paths, ProviderId::Codex, "fixture").unwrap();
    let child = spawn(&paths, &id, 20_000);
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let r = launch::status(&paths, &id).unwrap();
        if r.detail.as_deref() == Some(launch::ACCOUNT_WAIT_DETAIL) {
            assert_eq!(r.phase, "starting_provider");
            break;
        }
        assert!(Instant::now() < until, "no waiting status: {r:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(archive);
    let (code, stderr) = finish(child);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr
            .trim_end()
            .starts_with("sessionsation launch failed: ACCOUNT_REMOVED"),
        "{stderr}"
    );
    assert_failed_and_released(&paths, &id, "waits-name", "ACCOUNT_REMOVED");
}
