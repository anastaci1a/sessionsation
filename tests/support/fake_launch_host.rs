//! Drive the real UI launch path (`api launch start` -> HERDR pane ->
//! `launch-run` -> provider exec) against a scripted HERDR host.
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const HOST: &str = r#"#!/bin/sh
[ "$HERDR_SOCKET_PATH" = "$FIXTURE_DIR/host.sock" ] || exit 41
[ "$HERDR_SESSION" = fixture ] || exit 41
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
"#;

/// Kills the provider process group started in the scripted pane.
pub struct LaunchedPane {
    group: PathBuf,
    _host: std::os::unix::net::UnixListener,
}
impl Drop for LaunchedPane {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.group) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                }
            }
        }
    }
}

/// Start `account` through `api launch start` and return the pane guard once
/// the launch has dispatched into its pane.
pub fn launch(binary: &str, root: &Path, dir: &Path, account: &str, name: &str) -> LaunchedPane {
    let socket = dir.join("host.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let script = dir.join("herdr");
    std::fs::write(&script, HOST).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let request = json!({"version":1,"request_id":uuid::Uuid::new_v4().to_string(),"account_id":account,"name":name,
        "host":{"session":"fixture","socket":socket},"space":{"kind":"existing","id":"w1"},
        "directory":{"path":dir.join(name),"create":[dir.join(name)],"confirmed_existing":false}});
    let api = |args: &[&str], body: Option<&Value>| {
        let mut child = Command::new(binary)
            .arg("--data-dir")
            .arg(root)
            .arg("api")
            .args(args)
            .env(
                "PATH",
                format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
            )
            .env("FIXTURE_DIR", dir)
            .env("XDG_CONFIG_HOME", dir.join("host-config"))
            .env("HERDR_CONFIG_PATH", dir.join("host.toml"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("HERDR_ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        if let Some(body) = body {
            stdin.write_all(body.to_string().as_bytes()).unwrap();
        }
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    };
    api(&["launch", "start"], Some(&request));
    let id = request["request_id"].as_str().unwrap();
    let pane = LaunchedPane {
        group: dir.join("group.pid"),
        _host: listener,
    };
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let receipt = api(&["launch", "status", "--request-id", id], None);
        if matches!(
            receipt["phase"].as_str(),
            Some("starting_provider" | "running")
        ) {
            return pane;
        }
        assert!(
            Instant::now() < until,
            "{receipt}; {:?}",
            std::fs::read_to_string(dir.join("pane-output"))
        );
        std::thread::sleep(Duration::from_millis(80));
    }
}
