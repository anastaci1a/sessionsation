//! Contract 1.62.1: the usage meter of an idle Codex account stays fresh with no session
//! running, an on-demand read refreshes an old cache, and a read that is unsafe (a running
//! session holds the account lease and no shared app-server serves it) leaves the meter
//! stale with the reason, never `fresh:true`. A fake app-server, disposable homes.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use fs2::FileExt;
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGKILL);
            }
        }
        let _ = self.0.wait();
    }
}
fn ok(root: &Path, args: &[&str]) -> Value {
    let out = Command::new(binary())
        .arg("--data-dir")
        .arg(root)
        .arg("api")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["result"].clone()
}
struct Fixture {
    _tmp: tempfile::TempDir,
    paths: Paths,
    _daemon: Process,
}
impl Fixture {
    /// `idle_poll`: the supervisor's idle cadence; `cache`: the on-demand cache period.
    async fn new(idle_poll: u64, cache: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let codex = tmp.path().join("fake-codex");
        fs::write(&codex, include_str!("support/fake-codex.sh")).unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        sessionsation::store::save_config(
            &paths,
            &sessionsation::config::Config {
                codex_binary: codex.to_str().unwrap().into(),
                idle_poll_seconds: idle_poll,
                active_poll_seconds: idle_poll.min(30),
                near_limit_poll_seconds: idle_poll.min(10),
                stale_seconds: idle_poll + 90,
                ..Default::default()
            },
        )
        .unwrap();
        let daemon = Process(
            Command::new(binary())
                .arg("--data-dir")
                .arg(&paths.root)
                .args(["daemon", "serve"])
                .env("SESSIONSATION_CODEX_METER_CACHE_SECONDS", cache)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon::call(&paths, "status", json!({})).await.is_err() {
            assert!(Instant::now() < deadline, "supervisor did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self {
            _tmp: tmp,
            paths,
            _daemon: daemon,
        }
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join(format!("accounts/codex/{name}"))
    }
    async fn add(&self, name: &str) {
        daemon::call(&self.paths, "account/add", json!({"name":name}))
            .await
            .unwrap();
    }
    async fn meter(&self, name: &str) -> Value {
        let v = daemon::call(&self.paths, "status", json!({}))
            .await
            .unwrap();
        v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == name)
            .unwrap()["usage"]["meter"]
            .clone()
    }
    async fn until_short(&self, name: &str, short: f64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let m = self.meter(name).await;
            if m["fresh"] == true && m["windows"]["five_hour"]["used_percent"] == short {
                return m;
            }
            assert!(
                Instant::now() < deadline,
                "no fresh reading of {short}: {m}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
fn row<'a>(usage: &'a Value, id: &str) -> &'a Value {
    usage["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["account_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {usage}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_account_with_no_session_keeps_getting_fresh_readings() {
    let f = Fixture::new(1, "60").await;
    f.add("first").await;
    f.until_short("first", 23.0).await;
    // The provider's numbers move; with no session anywhere the meter follows them.
    fs::write(f.home("first").join("fake-short"), "41").unwrap();
    let m = f.until_short("first", 41.0).await;
    assert_eq!(m["source"], "app_server");
    assert_eq!(m["requires_session"], false);
    assert!(m["reason"].is_null(), "a fresh reading has no reason: {m}");
    fs::write(f.home("first").join("fake-short"), "52").unwrap();
    f.until_short("first", 52.0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_on_demand_read_refreshes_a_cache_older_than_the_cache_period() {
    // The background cadence is an hour, so only an on-demand read can refresh.
    let f = Fixture::new(3600, "2").await;
    let root = f.paths.root.clone();
    f.add("first").await;
    f.until_short("first", 23.0).await;
    let home = f.home("first");

    // Within the cache period nothing is read.
    fs::write(home.join("fake-short"), "61").unwrap();
    let usage = ok(&root, &["usage", "--account", "codex:first"]);
    assert!(row(&usage, "codex:first")["read"].is_null(), "{usage}");
    assert_eq!(row(&usage, "codex:first")["short"]["used_percent"], 23.0);

    // Older than the period: `api usage` reads live.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let usage = ok(&root, &["usage", "--account", "codex:first"]);
    let r = row(&usage, "codex:first");
    assert_eq!(r["read"]["attempted"], true, "{r}");
    assert_eq!(r["read"]["ok"], true, "{r}");
    assert_eq!(r["short"]["used_percent"], 61.0, "{r}");
    assert_eq!(r["meter"]["windows"]["five_hour"]["used_percent"], 61.0);
    assert_eq!(r["meter"]["fresh"], true);

    // `api account limits` reads live the same way.
    fs::write(home.join("fake-short"), "70").unwrap();
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let limits = ok(&root, &["account", "limits", "codex:first"]);
    assert_eq!(
        limits["usage"]["meter"]["windows"]["five_hour"]["used_percent"], 70.0,
        "{limits}"
    );
    assert_eq!(limits["meter_read"]["ok"], true, "{limits}");

    // `--cached` never reads.
    fs::write(home.join("fake-short"), "80").unwrap();
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let usage = ok(&root, &["usage", "--account", "codex:first", "--cached"]);
    assert_eq!(row(&usage, "codex:first")["short"]["used_percent"], 70.0);
    assert!(row(&usage, "codex:first")["read"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsafe_read_leaves_the_meter_stale_and_says_why() {
    let f = Fixture::new(1, "0").await;
    let root = f.paths.root.clone();
    // A running session holds the account's lease; no shared app-server serves the account.
    let home = f.home("first");
    fs::create_dir_all(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let lease = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join(".switcher-lease"))
        .unwrap();
    lease.lock_exclusive().unwrap();
    f.add("first").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let m = f.meter("first").await;
    assert_eq!(m["fresh"], false, "{m}");
    let reason = m["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("lease"), "the reason names the lease: {m}");
    // An on-demand read does not force one either, and says the same.
    let usage = ok(&root, &["usage", "--account", "codex:first"]);
    let r = row(&usage, "codex:first");
    assert_eq!(r["read"]["attempted"], false, "{r}");
    assert_eq!(r["meter"]["fresh"], false, "{r}");
    assert!(
        r["meter"]["reason"].as_str().unwrap().contains("lease"),
        "{r}"
    );
    let limits = ok(&root, &["account", "limits", "codex:first"]);
    assert_eq!(limits["usage"]["meter"]["fresh"], false);
    assert!(limits["usage"]["meter"]["reason"].is_string(), "{limits}");

    // The session ends: the next read is safe and the reason goes away.
    FileExt::unlock(&lease).unwrap();
    f.until_short("first", 23.0).await;
    assert!(f.meter("first").await["reason"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_supervisor_keeps_the_cached_state_fresh_with_no_full_state_caller() {
    // Compressed time: the 20 s refresh period is one second here.
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let daemon = Process(
        Command::new(binary())
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["daemon", "serve"])
            .env("SESSIONSATION_TEST_TIME_SCALE", "20")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon::call(&paths, "status", json!({})).await.is_err() {
        assert!(Instant::now() < deadline, "supervisor did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let observed =
        |_: ()| ok(&paths.root, &["state", "--cached"])["snapshot"]["observed_at"].as_i64();
    // The startup snapshot, written by the supervisor.
    let deadline = Instant::now() + Duration::from_secs(10);
    let first = loop {
        if let Some(at) = observed(()) {
            break at;
        }
        assert!(Instant::now() < deadline, "no startup snapshot");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // Nobody runs a full `state`; the cached observation still advances, and says it is fresh.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let v = ok(&paths.root, &["state", "--cached"]);
        if v["snapshot"]["observed_at"].as_i64().unwrap() > first {
            assert_eq!(v["snapshot"]["fresh"], true);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the cached state never advanced past {first}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    drop(daemon);
}
