#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
//! Contract 1.68.1: the detached `launch-worker` and the receipt reconcile that judges it.
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    id: String,
}
impl Fixture {
    /// An `admitted` receipt as `api launch start` journals it before spawning the worker.
    /// The account does not exist, so a worker that runs ends it `failed` in validation.
    fn admitted(edit: impl FnOnce(&mut Value)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        sessionsation::paths::Paths::new(Some(root.clone())).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut receipt = json!({"version":1,"request":{"version":1,"request_id":id,
            "account_id":"codex:absent","name":"probe","host":{"session":"fixture",
            "socket":tmp.path().join("host.sock")},"space":{"kind":"existing","id":"w1"},
            "directory":{"path":project,"create":[],"confirmed_existing":true}},
            "phase":"admitted","detail":null,"workspace_id":null,"tab_id":null,"pane_id":null,
            "conversation_id":null,"session_id":null,"generation":null,"created_directories":[],
            "created_at":now(),"updated_at":now()});
        edit(&mut receipt);
        let f = Self {
            _tmp: tmp,
            root,
            id,
        };
        std::fs::create_dir_all(f.file("json").parent().unwrap()).unwrap();
        std::fs::write(f.file("json"), receipt.to_string()).unwrap();
        f
    }
    fn file(&self, extension: &str) -> PathBuf {
        self.root
            .join(sessionsation::layout::LAUNCHES)
            .join(format!("{}.{extension}", self.id))
    }
    fn receipt(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.file("json")).unwrap()).unwrap()
    }
    /// `api launch status`: the reconcile every client poll runs.
    fn status(&self) -> Value {
        let out = Command::new(binary())
            .arg("--data-dir")
            .arg(&self.root)
            .args(["api", "launch", "status", "--request-id", &self.id])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    }
    fn wait_phase(&self, done: impl Fn(&str) -> bool) -> Value {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let r = self.receipt();
            if done(r["phase"].as_str().unwrap()) {
                return r;
            }
            assert!(Instant::now() < until, "{r}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
fn start_time(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}
fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < until, "the worker did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A status probe (`worker_stopped`) takes the worker lock for an instant. A worker that
/// starts in that instant waits for it instead of exiting with `EWOULDBLOCK`, so the probe
/// can never turn a live launch into "Worker exited before terminal handoff".
#[test]
fn a_status_probe_holding_the_lock_cannot_make_the_worker_fail() {
    let f = Fixture::admitted(|_| {});
    let probe = sessionsation::paths::lock_file(&f.file("worker.lock")).unwrap();
    probe.lock_exclusive().unwrap();
    let mut worker = Command::new(binary())
        .arg("--data-dir")
        .arg(&f.root)
        .args(["launch-worker", &f.id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let early = worker.try_wait().unwrap();
    probe.unlock().unwrap();
    let status = wait_exit(&mut worker);
    assert!(
        early.is_none(),
        "the worker gave up while a probe held its lock"
    );
    assert!(status.success(), "{status}");
    let r = f.receipt();
    assert_eq!(r["phase"], "failed", "{r}");
    let detail = r["detail"].as_str().unwrap();
    assert!(!detail.contains("Worker exited"), "{detail}");
}

/// A recorded worker that is still running but has not taken its lock yet (a loaded host)
/// is starting, not dead: more than 10 s after admission the launch stays `admitted`.
#[test]
fn a_slow_starting_worker_is_not_marked_failed() {
    let mut slow = Command::new("sleep").arg("30").spawn().unwrap();
    let (pid, start) = (slow.id(), start_time(slow.id()));
    let f = Fixture::admitted(|r| {
        r["created_at"] = json!(now() - 30);
        r["updated_at"] = json!(now() - 30);
        r["worker"] = json!({"pid":pid,"start":start});
    });
    let r = f.status();
    let _ = slow.kill();
    let _ = slow.wait();
    assert_eq!(r["phase"], "admitted", "{r}");
    assert_eq!(r["detail"], Value::Null, "{r}");
    // Once that exact process is gone the old rule applies again.
    assert_eq!(f.status()["phase"], "failed");
}

/// The live defect (6eaa7f8b, contract 1.67.1): the client's `timeout(1)` killed the
/// process group of `session launch` after it journaled the `admitted` receipt but before
/// its worker left that group, so no worker ever ran and the status poll 12 s later
/// reported "Worker exited before terminal handoff". Nothing was done in that state, so
/// the reconcile dispatches it once more and the worker's own outcome is reported.
#[test]
fn an_admitted_launch_whose_spawner_died_is_dispatched_once_more() {
    let f = Fixture::admitted(|r| r["updated_at"] = json!(now() - 12));
    let r = f.status();
    assert_eq!(r["phase"], "admitted", "{r}");
    assert!(r["worker"]["pid"].as_u64().is_some(), "{r}");
    let r = f.wait_phase(|p| p == "failed");
    let detail = r["detail"].as_str().unwrap();
    assert!(!detail.contains("Worker exited"), "{detail}");
    assert!(f.file("worker.lock").exists());
    assert!(f.file("worker.log").exists());
}

/// A worker that dies before its lock leaves its error in `<id>.worker.log`; a launch whose
/// recorded worker died that way is not dispatched again but fails.
#[test]
fn a_launch_worker_that_dies_early_leaves_its_stderr_in_the_log() {
    let f = Fixture::admitted(|r| r["updated_at"] = json!(now() - 12));
    // The worker opens its lock with O_NOFOLLOW: a symlink makes it fail before the lock.
    std::os::unix::fs::symlink(f.root.join("elsewhere"), f.file("worker.lock")).unwrap();
    let r = f.status();
    let worker = r["worker"].clone();
    assert_eq!(r["phase"], "admitted", "{r}");
    let until = Instant::now() + Duration::from_secs(30);
    let log = loop {
        let log = std::fs::read_to_string(f.file("worker.log")).unwrap_or_default();
        if log.contains("\"ok\":false") {
            break log;
        }
        assert!(
            Instant::now() < until,
            "no worker error in the log: {log:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(log.contains("symbolic links"), "{log}");
    assert!(log.contains(" started by "), "{log}");
    let mut r = f.receipt();
    r["updated_at"] = json!(now() - 12);
    std::fs::write(f.file("json"), r.to_string()).unwrap();
    let r = f.status();
    assert_eq!(r["phase"], "failed", "{r}");
    assert_eq!(r["worker"], worker, "dispatched more than once: {r}");
    assert!(Path::new(&f.file("worker.log")).exists());
}
