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
