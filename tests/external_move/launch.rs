use super::*;
use std::{os::unix::fs::PermissionsExt, process::Stdio};
struct OwnedGroup(PathBuf);
impl Drop for OwnedGroup {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.0)
            .and_then(|s| s.trim().parse::<i32>().map_err(std::io::Error::other))
        {
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
    }
}
fn request(f: &Fixture, host: &Path, name: &str) -> Value {
    json!({"version":1,"request_id":uuid::Uuid::new_v4().to_string(),"account_id":"codex:personal","name":name,"host":{"session":"fixture","socket":host},"space":{"kind":"existing","id":"w1"},"directory":{"path":f.tmp.path().join(name),"create":[f.tmp.path().join(name)],"confirmed_existing":false}})
}
fn api(f: &Fixture, args: &[&str], request: Option<&Value>) -> Output {
    let mut child = Command::new(binary())
        .arg("--data-dir")
        .arg(&f.paths.root)
        .arg("api")
        .args(args)
        .env(
            "PATH",
            format!(
                "{}:{}",
                f.tmp.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("FIXTURE_DIR", f.tmp.path())
        .env("XDG_CONFIG_HOME", f.tmp.path().join("host-config"))
        .env("HERDR_CONFIG_PATH", f.tmp.path().join("host.toml"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(r) = request {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(r.to_string().as_bytes())
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    child.wait_with_output().unwrap()
}

struct ActualHost {
    process: std::process::Child,
    config: PathBuf,
}
impl ActualHost {
    fn call(&self, args: &[&str]) -> Value {
        let out = Command::new("herdr")
            .args(["--session", "fixture"])
            .args(args)
            .env("XDG_CONFIG_HOME", &self.config)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(v.get("error").is_none(), "{v}");
        v.get("result").unwrap_or(&v).clone()
    }
}
impl Drop for ActualHost {
    fn drop(&mut self) {
        let _ = Command::new("herdr")
            .args(["--session", "fixture", "server", "stop"])
            .env("XDG_CONFIG_HOME", &self.config)
            .output();
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Run only in the documented network-disabled container with an explicit HERDR
/// binary mount; the provider and every account remain synthetic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated HERDR binary; see docs/operations.md"]
async fn installed_herdr_new_tab_handoff_retains_exact_account_and_shell() {
    assert_eq!(
        std::env::var("SESSIONSATION_ISOLATED_HOST_TEST").as_deref(),
        Ok("1")
    );
    let f = Fixture::new().await;
    let config = f.tmp.path().join("host-config");
    let file = f.tmp.path().join("host.toml");
    fs::write(&file, "[terminal]\ndefault_shell = \"/bin/bash\"\n").unwrap();
    let log = fs::File::create(f.tmp.path().join("herdr.log")).unwrap();
    let process = Command::new("herdr")
        .args(["--session", "fixture", "server"])
        .env("XDG_CONFIG_HOME", &config)
        .env("HERDR_CONFIG_PATH", &file)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let host = ActualHost { process, config };
    let socket = host.config.join("herdr/sessions/fixture/herdr.sock");
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        socket.exists(),
        "{}",
        fs::read_to_string(f.tmp.path().join("herdr.log")).unwrap()
    );
    let original = host.call(&[
        "workspace",
        "create",
        "--label",
        "Origin",
        "--cwd",
        f.tmp.path().to_str().unwrap(),
    ]);
    let workspace = original["workspace"]["workspace_id"].as_str().unwrap();
    let origin_pane = original["root_pane"]["pane_id"].as_str().unwrap();
    let origin_before = host.call(&["pane", "process-info", "--pane", origin_pane]);
    let mut r = request(&f, &socket, "Owned popup fixture");
    r["space"]["id"] = json!(workspace);
    let id = r["request_id"].as_str().unwrap();
    let preview = api(&f, &["launch", "validate"], Some(&r));
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(!f.tmp.path().join("Owned popup fixture").exists());
    let start = api(&f, &["launch", "start"], Some(&r));
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let until = Instant::now() + Duration::from_secs(40);
    let receipt = loop {
        let output = api(&f, &["launch", "status", "--request-id", id], None);
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        let r = v["result"].clone();
        if r["phase"] == "running" {
            break r;
        }
        assert!(
            Instant::now() < until,
            "{r}; {}",
            fs::read_to_string(f.tmp.path().join("herdr.log")).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
    };
    let pane = receipt["pane_id"].as_str().unwrap();
    assert_ne!(pane, origin_pane);
    assert_eq!(receipt["workspace_id"], workspace);
    assert_eq!(receipt["session_id"], id);
    assert_eq!(
        host.call(&["pane", "process-info", "--pane", origin_pane])["process_info"]["shell_pid"],
        origin_before["process_info"]["shell_pid"]
    );
    let args =
        fs::read_to_string(f.paths.account("personal").unwrap().join("launch-args")).unwrap();
    assert!(args.contains(receipt["conversation_id"].as_str().unwrap()));
    assert!(args.contains(f.paths.account("personal").unwrap().to_str().unwrap()));
    assert!(api(&f, &["launch", "start"], Some(&r)).status.success());
    assert_eq!(
        f.a.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "thread/start")
            .count(),
        1
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/start" || m == "turn/steer")
    );
    let info = host.call(&["pane", "process-info", "--pane", pane]);
    let shell_pid = info["process_info"]["shell_pid"].as_i64().unwrap();
    let pid = info["process_info"]["foreground_processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "codex")
        .expect("inert provider foreground")["pid"]
        .as_i64()
        .unwrap();
    // Stop only the synthetic provider owned by this test, keeping the host shell.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = host.call(&["pane", "process-info", "--pane", pane]);
    assert_eq!(after["process_info"]["shell_pid"], shell_pid);
    assert_eq!(
        after["process_info"]["foreground_processes"][0]["pid"],
        shell_pid
    );
    println!(
        "installed HERDR: exact workspace/new tab/account/UUID/socket, no duplicate launch, origin unchanged, retained shell; model turns=0"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_popup_request_is_staged_named_and_never_replayed() {
    let f = Fixture::new().await;
    let socket = f.tmp.path().join("host.sock");
    let _socket = UnixListener::bind(&socket).unwrap();
    let _owned = OwnedGroup(f.tmp.path().join("group.pid"));
    let script = f.tmp.path().join("herdr");
    fs::write(&script,r#"#!/bin/sh
shift 2
printf '%s\n' "$*" >> "$FIXTURE_DIR/host-calls"
case "$1 $2" in
 'workspace list') printf '%s\n' '{"result":{"workspaces":[{"workspace_id":"w1","label":"fixture"}]}}';;
 'tab create') printf '%s\n' '{"result":{"root_pane":{"pane_id":"w1:p2"},"tab":{"tab_id":"w1:t2","workspace_id":"w1"}}}';;
 'pane process-info') printf '%s\n' '{"result":{"process_info":{"shell_pid":100,"foreground_processes":[{"pid":100,"name":"bash"}]}}}';;
 'pane run')
   shift 3
   HERDR_ENV=1 HERDR_PANE_ID=w1:p2 setsid sh -c "$1" </dev/null >"$FIXTURE_DIR/pane-output" 2>&1 &
   echo $! > "$FIXTURE_DIR/group.pid";;
 'tab focus') printf '%s\n' '{"result":{"type":"ok"}}';;
 *) exit 42;;
esac
"#).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let r = request(&f, &socket, "Popup name");
    let id = r["request_id"].as_str().unwrap();
    let preview = api(&f, &["launch", "validate"], Some(&r));
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(!f.tmp.path().join("Popup name").exists());
    assert!(
        !fs::read_to_string(f.tmp.path().join("host-calls"))
            .unwrap()
            .contains("tab create")
    );
    let start = api(&f, &["launch", "start"], Some(&r));
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(api(&f, &["launch", "start"], Some(&r)).status.success());
    let until = Instant::now() + Duration::from_secs(12);
    let receipt = loop {
        let output = api(&f, &["launch", "status", "--request-id", id], None);
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        let r = v["result"].clone();
        if r["phase"] == "running" {
            break r;
        }
        assert!(
            Instant::now() < until,
            "{r}; {:?}",
            fs::read_to_string(f.tmp.path().join("pane-output"))
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
    };
    assert_eq!(receipt["workspace_id"], "w1");
    assert_eq!(receipt["session_id"], id);
    let session = sessionsation::catalog::list(&f.paths)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap();
    assert_eq!(session.name, "Popup name");
    assert_eq!(session.account, "personal");
    // A worker timeout can precede provider exec. Reconcile evidence only using
    // the original request, without repeating native or terminal dispatch.
    let receipt_path = f.paths.root.join("launches").join(format!("{id}.json"));
    let mut late: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    late["phase"] = json!("uncertain");
    fs::write(&receipt_path, late.to_string()).unwrap();
    let reconciled = api(&f, &["launch", "status", "--request-id", id], None);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).unwrap();
    assert_eq!(reconciled["result"]["phase"], "running");
    assert_eq!(
        f.a.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "thread/start")
            .count(),
        1
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/start" || m == "turn/steer")
    );
    assert_eq!(
        fs::read_to_string(f.tmp.path().join("host-calls"))
            .unwrap()
            .lines()
            .filter(|s| s.starts_with("tab create"))
            .count(),
        1
    );
    let mut conflict = r.clone();
    conflict["name"] = json!("different");
    let out = api(&f, &["launch", "start"], Some(&conflict));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("REQUEST_ID_CONFLICT"));
    let late = request(&f, &socket, "late");
    assert!(
        api(&f, &["launch", "validate"], Some(&late))
            .status
            .success()
    );
    fs::create_dir(f.tmp.path().join("late")).unwrap();
    let out = api(&f, &["launch", "start"], Some(&late));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("DIRECTORY_ALREADY_EXISTS"));
}
