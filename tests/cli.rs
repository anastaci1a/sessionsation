use serde_json::Value;
use std::{
    process::{Command, Stdio},
    time::Duration,
};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
#[test]
fn reserved_launcher_is_honestly_gated_and_does_not_start_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let out = Command::new(binary())
        .args(["--data-dir"])
        .arg(&root)
        .args(["run", "--", "--dangerously-bypass-approvals-and-sandbox"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("NATIVE_HANDOFF_UNVERIFIED"));
    assert!(!root.join("registry.db").exists());
}
#[test]
fn offline_status_is_machine_readable_without_starting_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(binary())
        .arg("--data-dir")
        .arg(tmp.path().join("state"))
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["daemon"]["running"], false);
    assert_eq!(v["capabilities"]["managed_launch"], false);
}
struct Process(std::process::Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
        }
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn enrollment_monitoring_and_duplicate_identity_use_isolated_homes() {
    use serde_json::json;
    use sessionsation::{daemon, paths::Paths};
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-codex");
    std::fs::write(&fake, include_str!("support/fake-codex.sh")).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: fake.to_str().unwrap().into(),
        ..Default::default()
    };
    sessionsation::store::save_config(&paths, &config).unwrap();
    let mut _server = Process(
        Command::new(binary())
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["daemon", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if daemon::call(&paths, "status", json!({})).await.is_ok() {
            break;
        }
        assert!(
            _server.0.try_wait().unwrap().is_none(),
            "fixture supervisor exited before readiness"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "fixture supervisor did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let login = daemon::call(&paths, "account/add", json!({"name":"first"}))
        .await
        .unwrap();
    assert_eq!(login["login"]["userCode"], "TEST-ONLY");
    let mut observed = false;
    for _ in 0..100 {
        let v = daemon::call(&paths, "status", json!({})).await.unwrap();
        if v["accounts"][0]["usage_fresh"] == true {
            assert_eq!(v["accounts"][0]["usage"]["weekly"]["used_percent"], 42.);
            assert!(!v.to_string().contains("test-shared-identity"));
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(observed, "account polling did not populate usage");
    assert!(paths.root.join("accounts/first/auth.json").exists());
    daemon::call(&paths, "account/add", json!({"name":"second"}))
        .await
        .unwrap();
    let mut rejected = false;
    for _ in 0..100 {
        let v = daemon::call(&paths, "status", json!({})).await.unwrap();
        if v["accounts"][1]["enabled"] == false {
            rejected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        rejected,
        "duplicate account identity must not create another capacity pool"
    );
    assert!(
        daemon::call(&paths, "session/run", json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("NATIVE_HANDOFF_UNVERIFIED")
    );
}
#[test]
fn daemon_configuration_singleton_and_status_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state with spaces");
    let paths = sessionsation::paths::Paths::new(Some(root.clone())).unwrap();
    let c = sessionsation::config::Config {
        codex_binary: "/bin/false".into(),
        ..Default::default()
    };
    sessionsation::store::save_config(&paths, &c).unwrap();
    let mut server = Process(
        Command::new(binary())
            .arg("--data-dir")
            .arg(&root)
            .args(["daemon", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut ready = false;
    for _ in 0..100 {
        let out = Command::new(binary())
            .arg("--data-dir")
            .arg(&root)
            .args(["status", "--json"])
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["daemon"]["running"] == true {
            ready = true;
            assert_eq!(v["capabilities"]["automatic_switching"], false);
            break;
        }
        assert!(server.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready);
    let second = Command::new(binary())
        .arg("--data-dir")
        .arg(&root)
        .args(["daemon", "serve"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    let changed = Command::new(binary())
        .arg("--data-dir")
        .arg(&root)
        .args(["config", "set", "balance_increment", "7"])
        .output()
        .unwrap();
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let invalid = Command::new(binary())
        .arg("--data-dir")
        .arg(&root)
        .args(["config", "set", "weekly_ceiling", "101"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let settings = Command::new(binary())
        .arg("--data-dir")
        .arg(&root)
        .args(["config", "get"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&settings.stdout).unwrap();
    assert_eq!(v["balance_increment"], 7.);
    assert_eq!(v["weekly_ceiling"], 90.);
    let stopped = Command::new(binary())
        .arg("--data-dir")
        .arg(&root)
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let v: Value = serde_json::from_slice(&stopped.stdout).unwrap();
    assert_eq!(v["stopping"], true);
    for _ in 0..100 {
        if server.0.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("daemon failed to exit after acknowledging stop");
}

#[test]
fn public_state_watch_flushes_complete_snapshots_and_stops_independently() {
    use std::io::{BufRead, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let mut watcher = Process(
        Command::new(binary())
            .arg("--data-dir")
            .arg(dir.path())
            .args(["api", "state", "--watch", "--interval", "1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut reader = BufReader::new(watcher.0.stdout.take().unwrap());
    for _ in 0..2 {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["api_version"], 1);
        assert_eq!(value["ok"], true);
        assert!(value["result"]["accounts"].is_array());
        assert!(value["result"]["sessions"].is_array());
    }
    drop(watcher);
    assert!(!dir.path().join("registry.db").exists());
}

#[test]
fn optional_service_setup_preserves_original_switcher_service() {
    let dir = tempfile::tempdir().unwrap();
    let services = dir.path().join(".config/systemd/user");
    std::fs::create_dir_all(&services).unwrap();
    let original = services.join("codex-switcher.service");
    std::fs::write(&original, "original service").unwrap();
    let result = Command::new(binary())
        .env("HOME", dir.path())
        .arg("--data-dir")
        .arg(dir.path().join("private-state"))
        .args(["setup", "--systemd"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(original).unwrap(),
        "original service"
    );
    assert!(services.join("sessionsation.service").is_file());
    assert!(!dir.path().join("private-state/registry.db").exists());
}
