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

// ------------------------------------------- contract 1.73.1: a dead uncertain launch settles

/// An `uncertain` receipt holding the name `probe` in Space `fixture`, its worker `worker`.
fn uncertain(worker: Option<(u32, u64)>) -> Fixture {
    let f = Fixture::admitted(|r| {
        r["phase"] = json!("uncertain");
        r["detail"] = json!("LAUNCH_ACK_UNCERTAIN");
        r["space_label"] = json!("fixture");
        if let Some((pid, start)) = worker {
            r["worker"] = json!({"pid":pid,"start":start});
        }
    });
    let paths = sessionsation::paths::Paths::new(Some(f.root.clone())).unwrap();
    sessionsation::catalog::reserve_name(&paths, Some("fixture"), "probe", &f.id).unwrap();
    f
}
/// A worker identity (pid, start time) whose process has exited.
fn dead_worker() -> (u32, u64) {
    let mut child = Command::new("true").spawn().unwrap();
    let identity = (child.id(), start_time(child.id()));
    child.wait().unwrap();
    identity
}
/// `api launch start` of `request` against a scripted HERDR that only lists Space `fixture`.
fn start(f: &Fixture, request: &Value) -> (bool, String) {
    let dir = f.root.parent().unwrap();
    let socket = dir.join("host.sock");
    let _host = std::os::unix::net::UnixListener::bind(&socket);
    let script = dir.join("herdr");
    std::fs::write(&script, "#!/bin/sh\n[ \"$1 $2\" = 'workspace list' ] || exit 42\nprintf '%s\\n' '{\"result\":{\"workspaces\":[{\"workspace_id\":\"w1\",\"label\":\"fixture\"}]}}'\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let mut child = Command::new(binary())
        .arg("--data-dir")
        .arg(&f.root)
        .args(["api", "launch", "start"])
        .env(
            "PATH",
            format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}
/// The same request under a new request id: a retry of the launch with the same name.
fn retry(f: &Fixture) -> Value {
    let mut request = f.receipt()["request"].clone();
    request["request_id"] = json!(uuid::Uuid::new_v4().to_string());
    request
}

/// The live defect (2026-11-26): a launch went `uncertain`, its worker died and nothing ever
/// ran the session. The reconcile (`launch status`, the supervisor) settles it and frees the
/// name; the admission path is `a_retry_is_admitted_over_a_dead_uncertain_launch_…` (gemini).
#[test]
fn a_dead_uncertain_launch_settles_on_the_reconcile_and_frees_its_name() {
    let f = uncertain(Some(dead_worker()));
    let r = f.status();
    assert_eq!(r["phase"], "failed", "{r}");
    assert!(
        r["detail"]
            .as_str()
            .unwrap()
            .starts_with("settled: launch worker gone, no session was bound"),
        "{r}"
    );
    let paths = sessionsation::paths::Paths::new(Some(f.root.clone())).unwrap();
    sessionsation::catalog::reserve_name(&paths, Some("fixture"), "probe", "retry").unwrap();
    assert_eq!(f.status()["phase"], "failed");
}

/// Anything less certain keeps the name: no recorded worker, or a live process carrying the
/// session id (a provider's session env, or a `launch-run` argument). The refusal names the
/// uncertain launch and its abandon.
#[test]
fn a_live_or_unknown_uncertain_launch_still_refuses_the_name() {
    let refused = |f: &Fixture| {
        let (ok, text) = start(f, &retry(f));
        assert!(!ok && text.contains("SESSION_NAME_EXISTS"), "{text}");
        assert!(
            text.contains(&format!("held by the uncertain launch {}", f.id)),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "api abandon-operation --operation {0} --confirm {0}",
                f.id
            )),
            "{text}"
        );
        assert_eq!(f.receipt()["phase"], "uncertain");
    };
    refused(&uncertain(None));
    let f = uncertain(Some(dead_worker()));
    let env = sessionsation::provider::SESSION_ENV;
    let mut provider = Command::new("sleep")
        .arg("30")
        .env(env, &f.id)
        .spawn()
        .unwrap();
    let mut launch_run = Command::new("sh")
        .args(["-c", "sleep 30; :", "launch-run", &f.id])
        .spawn()
        .unwrap();
    refused(&f);
    let _ = provider.kill();
    let _ = provider.wait();
    refused(&f);
    let _ = launch_run.kill();
    let _ = launch_run.wait();
    assert_eq!(f.status()["phase"], "failed");
}

/// A retry with the request id of the uncertain record is the same request: it is replayed
/// as recorded (still `uncertain`, no new worker), never settled or dispatched by admission.
#[test]
fn a_retry_with_the_same_request_id_stays_idempotent() {
    let f = uncertain(Some(dead_worker()));
    let before = f.receipt();
    let (ok, text) = start(&f, &before["request"]);
    assert!(ok, "{text}");
    let after = f.receipt();
    assert_eq!(after["phase"], "uncertain", "{after}");
    assert_eq!(after["worker"], before["worker"]);
    assert_eq!(after["updated_at"], before["updated_at"]);
}
