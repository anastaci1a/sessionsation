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
pub(super) fn api(f: &Fixture, args: &[&str], request: Option<&Value>) -> Output {
    api_in(f, args, request, false)
}
fn api_in(f: &Fixture, args: &[&str], request: Option<&Value>, foreign: bool) -> Output {
    let mut command = Command::new(binary());
    command
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
        .stderr(Stdio::piped());
    if foreign {
        command
            .env("XDG_CONFIG_HOME", f.tmp.path().join("unrelated-host"))
            .env("HERDR_SOCKET_PATH", f.tmp.path().join("not-the-host.sock"));
    }
    let mut child = command.spawn().unwrap();
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

fn foreign_api(f: &Fixture, args: &[&str]) -> Output {
    api_in(f, args, None, true)
}

struct ActualHost {
    process: std::process::Child,
    config: PathBuf,
}
impl ActualHost {
    async fn start(f: &Fixture) -> Self {
        let config = f.tmp.path().join("host-config");
        let file = f.tmp.path().join("host.toml");
        fs::write(&file, "[terminal]\ndefault_shell = \"/bin/bash\"\n").unwrap();
        let log = fs::File::create(f.tmp.path().join("herdr.log")).unwrap();
        let process = Command::new("herdr")
            .args(["--session", "fixture", "server"])
            .env("XDG_CONFIG_HOME", &config)
            .env("HERDR_CONFIG_PATH", &file)
            .env("SESSIONSATION_FIXTURE_EXIT", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let host = Self { process, config };
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
        host
    }
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
        if out.stdout.is_empty() && args.starts_with(&["pane", "run"]) {
            return json!({"admitted":true});
        }
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

/// Presentation-only moves use an owned HERDR and an inert provider, including
/// a lost acknowledgement. No real host, account or model is accessed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated HERDR binary; see docs/operations.md"]
async fn installed_herdr_space_move_preserves_process_and_recovers_without_replay() {
    assert_eq!(
        std::env::var("SESSIONSATION_ISOLATED_HOST_TEST").as_deref(),
        Ok("1")
    );
    let f = Fixture::new().await;
    let host = ActualHost::start(&f).await;
    let socket = host.config.join("herdr/sessions/fixture/herdr.sock");
    let origin = host.call(&[
        "workspace",
        "create",
        "--label",
        "Origin",
        "--cwd",
        f.tmp.path().to_str().unwrap(),
    ]);
    let w1 = origin["workspace"]["workspace_id"].as_str().unwrap();
    let shell = origin["root_pane"]["pane_id"].as_str().unwrap();
    let shell_before = host.call(&["pane", "process-info", "--pane", shell]);
    let destination = host.call(&[
        "workspace",
        "create",
        "--label",
        "Destination",
        "--cwd",
        f.tmp.path().to_str().unwrap(),
    ]);
    let w2 = destination["workspace"]["workspace_id"].as_str().unwrap();
    let mut r = request(&f, &socket, "Space fixture");
    r["space"]["id"] = json!(w1);
    let id = r["request_id"].as_str().unwrap();
    let start = api(&f, &["launch", "start"], Some(&r));
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let until = Instant::now() + Duration::from_secs(40);
    let receipt = loop {
        let out = api(&f, &["launch", "status", "--request-id", id], None);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["result"]["phase"] == "running" {
            break v["result"].clone();
        }
        assert!(Instant::now() < until, "{v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let receipt_path = f
        .paths
        .root
        .join(sessionsation::layout::LAUNCHES)
        .join(format!("{id}.json"));
    let receipt_bytes = fs::read(&receipt_path).unwrap();
    let pane = receipt["pane_id"].as_str().unwrap();
    let terminal = host.call(&["pane", "get", pane])["pane"]["terminal_id"].clone();
    let sibling = host.call(&["pane", "split", pane, "--direction", "right", "--no-focus"]);
    let sibling_id = sibling["pane"]["pane_id"].as_str().unwrap();
    let sibling_before = host.call(&["pane", "process-info", "--pane", sibling_id]);
    let thread = receipt["conversation_id"].as_str().unwrap();
    let state = |f: &Fixture| -> Value {
        let out = foreign_api(f, &["state"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    };
    let before = state(&f);
    assert_eq!(before["capabilities"]["move_space"], true);
    assert_eq!(before["placement"]["state"], "observed");
    assert_eq!(before["placement"]["space"]["label"], "Origin");
    let batch: Value = serde_json::from_slice(&foreign_api(&f, &["placements"]).stdout).unwrap();
    assert_eq!(
        batch["result"]["memberships"][id]["space"],
        before["placement"]["space"]
    );
    assert!(!batch.to_string().contains(socket.to_str().unwrap()));
    f.a.lock().unwrap().threads.get_mut(thread).unwrap()["status"]["type"] = json!("active");
    let calls = f.a.lock().unwrap().calls.len();
    let generation = before["generation"].as_str().unwrap();
    let observed = foreign_api(
        &f,
        &[
            "observe",
            "--session",
            id,
            "--expected-generation",
            generation,
        ],
    );
    let observed: Value = serde_json::from_slice(&observed.stdout).unwrap();
    assert!(
        observed["result"]["runtime_generation"].is_string(),
        "{observed}"
    );
    let spaces = foreign_api(
        &f,
        &[
            "spaces",
            "--session",
            id,
            "--expected-generation",
            generation,
        ],
    );
    assert!(
        spaces.status.success(),
        "{}",
        String::from_utf8_lossy(&spaces.stderr)
    );
    let available: Value = serde_json::from_slice(&spaces.stdout).unwrap();
    assert_eq!(available["result"]["current_space_id"], w1);
    let move_id = uuid::Uuid::new_v4().to_string();
    let args = [
        "move-space",
        "--session",
        id,
        "--expected-generation",
        generation,
        "--space",
        w2,
        "--request-id",
        &move_id,
    ];
    let mut stale = args;
    stale[4] = "stale";
    assert!(!foreign_api(&f, &stale).status.success());
    assert!(
        !f.paths
            .root
            .join(sessionsation::layout::SPACE_MOVES)
            .join(format!("{move_id}.json"))
            .exists()
    );
    // Keep the host reply pending after its effect. The API caller must exit
    // with admission while its detached worker still owns the operation.
    let real = Command::new("sh")
        .args(["-c", "command -v herdr"])
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let wrapper = f.tmp.path().join("herdr");
    let hold = f.tmp.path().join("hold-move-reply");
    let applied = f.tmp.path().join("applied-move");
    fs::write(&hold, "1").unwrap();
    fs::write(&wrapper, format!("#!/bin/sh\nif [ \"$1\" = pane ] && [ \"$2\" = move ]; then\n {} \"$@\" > {} || exit 1\n n=0; while [ -f {} ]; do n=$((n+1)); [ \"$n\" -lt 200 ] || exit 1; sleep 0.05; done\n cat {}\n exit 0\nfi\nexec {} \"$@\"\n", quote(real.trim()), quote(&applied.to_string_lossy()), quote(&hold.to_string_lossy()), quote(&applied.to_string_lossy()), quote(real.trim()))).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let moved = foreign_api(&f, &args);
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let v: Value = serde_json::from_slice(&moved.stdout).unwrap();
    assert_eq!(v["result"]["phase"], "admitted", "{v}");
    let until = Instant::now() + Duration::from_secs(15);
    while !fs::read(&applied).is_ok_and(|b| !b.is_empty()) {
        assert!(Instant::now() < until, "Host move never applied");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let pending: Value = serde_json::from_slice(&foreign_api(&f, &args).stdout).unwrap();
    assert_eq!(pending["result"]["phase"], "dispatching");
    assert_eq!(pending["result"]["can_recover"], false);
    fs::remove_file(&hold).unwrap();
    let completed = wait_space_move(&f, &move_id, "complete").await;
    let v = json!({"result": completed});
    assert_eq!(v["result"]["pane"]["terminal_id"], terminal);
    let moved_pane = v["result"]["pane"]["pane_id"].as_str().unwrap();
    let after = state(&f);
    let observed_after = foreign_api(
        &f,
        &[
            "observe",
            "--session",
            id,
            "--expected-generation",
            after["generation"].as_str().unwrap(),
        ],
    );
    let observed_after: Value = serde_json::from_slice(&observed_after.stdout).unwrap();
    assert_eq!(
        observed_after["result"]["runtime_generation"],
        observed["result"]["runtime_generation"]
    );
    assert_eq!(
        host.call(&["pane", "process-info", "--pane", sibling_id])["process_info"]["shell_pid"],
        sibling_before["process_info"]["shell_pid"]
    );
    for key in ["id", "account_id", "conversation_id"] {
        assert_eq!(after[key], before[key]);
    }
    for key in ["pid", "process_start"] {
        assert_eq!(after["attachment"][key], before["attachment"][key]);
    }
    assert_ne!(after["generation"], before["generation"]);
    assert!(
        after["attachment"]["target"]
            .as_str()
            .unwrap()
            .ends_with(moved_pane)
    );
    assert_eq!(fs::read(&receipt_path).unwrap(), receipt_bytes);
    let repeat: Value = serde_json::from_slice(&foreign_api(&f, &args).stdout).unwrap();
    assert_eq!(repeat["result"]["pane"], v["result"]["pane"]);
    assert_eq!(
        host.call(&["pane", "process-info", "--pane", shell])["process_info"]["shell_pid"],
        shell_before["process_info"]["shell_pid"]
    );

    // Lose the reply after the actual second move. Recovery may read the host,
    // repair ownership and release the reservation, but must never replay it.
    let real = Command::new("sh")
        .args(["-c", "command -v herdr"])
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    let count = f.tmp.path().join("move-count");
    let flag = f.tmp.path().join("lose-ack");
    fs::write(&flag, "1").unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let wrapper = f.tmp.path().join("herdr");
    fs::write(&wrapper, format!("#!/bin/sh\nif [ \"$1\" = pane ] && [ \"$2\" = move ]; then\n echo move >> {}\n if [ -f {} ]; then\n {} \"$@\" > {}\n rm -- {}\n exit 1\n fi\nfi\nexec {} \"$@\"\n", quote(&count.to_string_lossy()), quote(&flag.to_string_lossy()), quote(real.trim()), quote(&f.tmp.path().join("lost-result").to_string_lossy()), quote(&flag.to_string_lossy()), quote(real.trim()))).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let second = uuid::Uuid::new_v4().to_string();
    assert_eq!(after["placement"]["space"]["label"], "Destination");
    assert_eq!(
        after["placement"]["host_id"],
        before["placement"]["host_id"]
    );
    assert_ne!(
        after["placement"]["space"]["id"],
        before["placement"]["space"]["id"]
    );
    let moved_gen = after["generation"].as_str().unwrap();
    let uncertain = foreign_api(
        &f,
        &[
            "move-space",
            "--session",
            id,
            "--expected-generation",
            moved_gen,
            "--space",
            w1,
            "--request-id",
            &second,
        ],
    );
    let uncertain: Value = serde_json::from_slice(&uncertain.stdout).unwrap();
    assert_eq!(uncertain["result"]["phase"], "admitted", "{uncertain}");
    wait_space_move(&f, &second, "uncertain").await;
    assert!(
        !foreign_api(
            &f,
            &[
                "switch",
                "--session",
                id,
                "--expected-generation",
                moved_gen,
                "--to",
                "codex:work"
            ]
        )
        .status
        .success()
    );
    let recovered = foreign_api(&f, &["recover", &second]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let recovered: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(recovered["result"]["phase"], "complete", "{recovered}");
    assert_eq!(fs::read_to_string(&count).unwrap().lines().count(), 1);
    let current = state(&f);
    assert_eq!(current["attachment"]["pid"], before["attachment"]["pid"]);
    let details = foreign_api(
        &f,
        &[
            "session",
            "--session",
            id,
            "--expected-generation",
            current["generation"].as_str().unwrap(),
        ],
    );
    let details: Value = serde_json::from_slice(&details.stdout).unwrap();
    assert_eq!(
        details["result"]["original_host"]["workspace_id"], w1,
        "{details}"
    );
    assert_eq!(fs::read(&receipt_path).unwrap(), receipt_bytes);
    assert!(
        !f.a.lock().unwrap().calls[calls..]
            .iter()
            .any(|(m, _)| matches!(
                m.as_str(),
                "turn/start" | "turn/steer" | "thread/unload" | "thread/resume"
            ))
    );

    // A subsequent account handoff must reuse the moved pane, including its
    // retained shell environment, rather than reverting to a default account.
    f.a.lock().unwrap().threads.get_mut(thread).unwrap()["status"]["type"] = json!("idle");
    let switch = foreign_api(
        &f,
        &[
            "switch",
            "--session",
            id,
            "--expected-generation",
            current["generation"].as_str().unwrap(),
            "--to",
            "codex:work",
        ],
    );
    assert!(
        switch.status.success(),
        "{}",
        String::from_utf8_lossy(&switch.stderr)
    );
    let switch: Value = serde_json::from_slice(&switch.stdout).unwrap();
    f.wait(switch["result"]["id"].as_str().unwrap(), "complete")
        .await;
    let transferred = state(&f);
    assert_eq!(transferred["account_id"], "codex:work");
    assert_eq!(transferred["conversation_id"], thread);
    assert_eq!(
        transferred["attachment"]["target"],
        current["attachment"]["target"]
    );
    assert!(find_rollout(&f.paths.account("work").unwrap(), thread).is_some());
    let stop = foreign_api(
        &f,
        &[
            "stop",
            "--session",
            id,
            "--expected-generation",
            transferred["generation"].as_str().unwrap(),
        ],
    );
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let stop: Value = serde_json::from_slice(&stop.stdout).unwrap();
    f.wait(stop["result"]["id"].as_str().unwrap(), "complete")
        .await;
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
    let host = ActualHost::start(&f).await;
    let socket = host.config.join("herdr/sessions/fixture/herdr.sock");
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
    let mut r = request(&f, &socket, "Owned popup");
    r["space"]["id"] = json!(workspace);
    let id = r["request_id"].as_str().unwrap();
    let preview = api_in(&f, &["launch", "validate"], Some(&r), true);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(!f.tmp.path().join("Owned popup").exists());
    let start = api_in(&f, &["launch", "start"], Some(&r), true);
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
    {
        let server = f.a.lock().unwrap();
        let start = server
            .calls
            .iter()
            .find(|(method, _)| method == "thread/start")
            .unwrap();
        assert_eq!(start.1["approvalPolicy"], "never");
        assert_eq!(start.1["permissions"], ":danger-full-access");
        assert!(start.1.get("sandbox").is_none());
    }
    // HERDR owns the live label in both directions; provider/process identity stays put.
    let tab_id = receipt["tab_id"].as_str().unwrap();
    let before_name = host.call(&["pane", "process-info", "--pane", pane]);
    host.call(&["tab", "rename", tab_id, "Host renamed"]);
    let inventory: Value = serde_json::from_slice(&api(&f, &["state"], None).stdout).unwrap();
    let session = inventory["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap();
    assert_eq!(session["name"], "Host renamed");
    assert_eq!(session["name_source"], "herdr_last_observed");
    assert_eq!(session["conversation_id"], receipt["conversation_id"]);
    let renamed = api(
        &f,
        &[
            "rename",
            "--session",
            id,
            "--expected-generation",
            session["generation"].as_str().unwrap(),
            "--name",
            "Owned popup",
        ],
        None,
    );
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let renamed: Value = serde_json::from_slice(&renamed.stdout).unwrap();
    assert_eq!(renamed["result"]["name"], "Owned popup");
    assert_eq!(
        host.call(&["tab", "get", tab_id])["tab"]["label"],
        "Owned popup"
    );
    let same = api(
        &f,
        &[
            "rename",
            "--session",
            id,
            "--expected-generation",
            renamed["result"]["generation"].as_str().unwrap(),
            "--name",
            "Owned popup",
        ],
        None,
    );
    assert!(
        same.status.success(),
        "{}",
        String::from_utf8_lossy(&same.stderr)
    );
    assert_eq!(
        host.call(&["pane", "process-info", "--pane", pane])["process_info"]["foreground_processes"],
        before_name["process_info"]["foreground_processes"]
    );
    let legacy = f.invoke(&[
        "rename",
        "--session",
        &format!("thread:{}", receipt["conversation_id"].as_str().unwrap()),
        "--name",
        "Legacy renamed",
    ]);
    assert_eq!(legacy["name"], "Legacy renamed");
    assert_eq!(
        host.call(&["tab", "get", tab_id])["tab"]["label"],
        "Legacy renamed"
    );
    host.call(&["tab", "rename", tab_id, "Owned popup"]);
    // Public relocation must restart only this owned pane and preserve identity.
    let changed = f.tmp.path().join("changed-root");
    fs::create_dir(&changed).unwrap();
    fs::write(changed.join("notes"), "retained project contents").unwrap();
    let moved = f.tmp.path().join("moved-root");
    for (destination, files) in [(&changed, false), (&moved, true)] {
        let out = api(&f, &["state"], None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let state: Value = serde_json::from_slice(&out.stdout).unwrap();
        let selected = state["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap();
        let mut args = vec![
            "relocate",
            "--session",
            id,
            "--expected-generation",
            selected["generation"].as_str().unwrap(),
            "--to",
            destination.to_str().unwrap(),
        ];
        if files {
            args.push("--move-files");
        }
        let out = api(&f, &args, None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let job: Value = serde_json::from_slice(&out.stdout).unwrap();
        let done = f
            .wait(job["result"]["id"].as_str().unwrap(), "complete")
            .await;
        assert_eq!(done["non_steering"], true);
        let out = api(&f, &["state"], None);
        let state: Value = serde_json::from_slice(&out.stdout).unwrap();
        let selected = state["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap();
        assert_eq!(selected["directory"], destination.to_str().unwrap());
        assert_eq!(selected["conversation_id"], receipt["conversation_id"]);
        assert_eq!(selected["account_id"], "codex:personal");
        assert_eq!(
            host.call(&["pane", "process-info", "--pane", origin_pane])["process_info"]["shell_pid"],
            origin_before["process_info"]["shell_pid"]
        );
    }
    assert!(!changed.exists());
    assert_eq!(
        fs::read_to_string(moved.join("notes")).unwrap(),
        "retained project contents"
    );
    // The subsequent missing-directory resume fixture deliberately removes it.
    fs::remove_file(moved.join("notes")).unwrap();
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
    // Normal /exit: no lifecycle Stop call and no signal. History must stay saved.
    host.call(&["pane", "run", pane, "/exit"]);
    for _ in 0..50 {
        if unsafe { libc::kill(pid as i32, 0) } != 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = host.call(&["pane", "process-info", "--pane", pane]);
    assert_eq!(after["process_info"]["shell_pid"], shell_pid);
    assert_eq!(
        after["process_info"]["foreground_processes"][0]["pid"],
        shell_pid
    );
    let stopped: Value = serde_json::from_slice(&api(&f, &["state"], None).stdout).unwrap();
    assert_eq!(
        stopped["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()["name"],
        "Owned popup"
    );
    // Leave a recorded unfinished turn, with no live writer. Consent must be explicit.
    let history = find_rollout(
        &f.paths.account("personal").unwrap(),
        receipt["conversation_id"].as_str().unwrap(),
    )
    .unwrap();
    writeln!(
        fs::OpenOptions::new().append(true).open(&history).unwrap(),
        "{}",
        json!({"type":"event_msg","payload":{"type":"task_started"}})
    )
    .unwrap();
    // Resume this same synthetic conversation twice: original account, then
    // transferred account. Every terminal belongs to this isolated HERDR server.
    let mut previous_directory = json!(moved);
    for (account, name) in [("personal", "Resumed orig"), ("work", "Transferred")] {
        let output = api(&f, &["state"], None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let state: Value = serde_json::from_slice(&output.stdout).unwrap();
        let saved = state["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap();
        assert_eq!(saved["availability"], "saved");
        assert!(saved["attachment"].is_null());
        assert_eq!(saved["conversation_id"], receipt["conversation_id"]);
        let mut resume = json!({"version":1,"request_id":uuid::Uuid::new_v4().to_string(),"session_id":id,
            "expected_generation":saved["generation"],"account_id":format!("codex:{account}"),"name":name,
            "host":{"session":"fixture","socket":socket},"space":{"kind":"existing","id":workspace}});
        assert_eq!(saved["resume_block_reason"], "unfinished_history");
        assert_eq!(saved["capabilities"]["resume"], true);
        let rejected = api(&f, &["resume-launch", "validate"], Some(&resume));
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("UNFINISHED_HISTORY_CONFIRMATION_REQUIRED")
        );
        resume["confirm_unfinished"] = json!(true);
        let destination = f.tmp.path().join(format!("resume-{account}"));
        resume["directory"] =
            json!({"path":destination,"create":[destination],"confirmed_existing":false});
        if account == "personal" {
            fs::remove_dir(previous_directory.as_str().unwrap()).unwrap();
            let state: Value = serde_json::from_slice(&api(&f, &["state"], None).stdout).unwrap();
            assert_eq!(
                state["result"]["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["id"] == id)
                    .unwrap()["capabilities"]["resume"],
                true
            );
        } else {
            resume["space"] = json!({"kind":"new","label":"Other Space"});
        }
        let request_id = resume["request_id"].as_str().unwrap();
        let preview = api(&f, &["resume-launch", "validate"], Some(&resume));
        assert!(
            preview.status.success(),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert!(
            !destination.exists(),
            "resume validation must not create directories"
        );
        let mut stale = resume.clone();
        stale["expected_generation"] = json!("stale");
        let rejected = api(&f, &["resume-launch", "start"], Some(&stale));
        assert!(!rejected.status.success());
        assert!(
            !f.paths
                .root
                .join(sessionsation::layout::RESUMPTIONS)
                .join(format!("{request_id}.json"))
                .exists()
        );
        assert!(
            !destination.exists(),
            "stale admission must not create directories"
        );
        let started = api(&f, &["resume-launch", "start"], Some(&resume));
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        assert!(
            api(&f, &["resume-launch", "start"], Some(&resume))
                .status
                .success()
        );
        let mut conflict = resume.clone();
        conflict["name"] = json!("Other intent");
        let rejected = api(&f, &["resume-launch", "start"], Some(&conflict));
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("REQUEST_ID_CONFLICT"));
        let until = Instant::now() + Duration::from_secs(45);
        let resumed = loop {
            let out = api(
                &f,
                &["resume-launch", "status", "--request-id", request_id],
                None,
            );
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let out: Value = serde_json::from_slice(&out.stdout).unwrap();
            let r = out["result"].clone();
            if r["phase"] == "running" {
                break r;
            }
            assert!(
                !matches!(r["phase"].as_str(), Some("failed" | "uncertain"))
                    && Instant::now() < until,
                "{r}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(resumed["conversation_id"], receipt["conversation_id"]);
        assert_eq!(resumed["directory"], json!(destination));
        assert_eq!(resumed["saved_directory"], previous_directory);
        previous_directory = json!(destination);
        assert_eq!(resumed["created_directories"], json!([destination]));
        if account == "personal" {
            assert_eq!(resumed["workspace_id"], workspace);
        } else {
            assert_ne!(resumed["workspace_id"], workspace);
            let spaces = host.call(&["workspace", "list"]);
            assert!(
                spaces["workspaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["workspace_id"] == resumed["workspace_id"]
                        && s["label"] == "Other Space")
            );
        }
        let state: Value = serde_json::from_slice(&api(&f, &["state"], None).stdout).unwrap();
        let running = state["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap();
        assert_eq!(running["name"], name);
        assert_eq!(running["account"], account);
        let foreign = foreign_api(&f, &["state"]);
        assert!(
            foreign.status.success(),
            "{}",
            String::from_utf8_lossy(&foreign.stderr)
        );
        let foreign: Value = serde_json::from_slice(&foreign.stdout).unwrap();
        let observed = foreign["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap();
        assert_eq!(observed["availability"], "running");
        assert_eq!(
            observed["generation"], running["generation"],
            "an observer with another HERDR socket must not invalidate the live attachment"
        );

        let calls = if account == "personal" { &f.a } else { &f.b };
        assert!(
            calls
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|(method, params)| method == "thread/resume"
                    && params["cwd"] == json!(destination)),
            "provider must receive the chosen directory"
        );
        let detail: Value = serde_json::from_slice(
            &api(
                &f,
                &[
                    "session",
                    "--session",
                    id,
                    "--expected-generation",
                    running["generation"].as_str().unwrap(),
                ],
                None,
            )
            .stdout,
        )
        .unwrap();
        assert_eq!(
            detail["result"]["original_host"]["pane_id"],
            resumed["pane_id"]
        );
        let new_pane = resumed["pane_id"].as_str().unwrap();
        assert_ne!(new_pane, pane);
        let info = host.call(&["pane", "process-info", "--pane", new_pane]);
        let agent = info["process_info"]["foreground_processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "codex")
            .unwrap()["pid"]
            .as_i64()
            .unwrap();
        {
            let state = if account == "personal" { &f.a } else { &f.b };
            state
                .lock()
                .unwrap()
                .threads
                .get_mut(receipt["conversation_id"].as_str().unwrap())
                .unwrap()["status"]["type"] = json!("notLoaded");
        }
        let stopped = foreign_api(
            &f,
            &[
                "stop",
                "--session",
                id,
                "--expected-generation",
                running["generation"].as_str().unwrap(),
            ],
        );
        assert!(
            stopped.status.success(),
            "{}",
            String::from_utf8_lossy(&stopped.stderr)
        );
        let stopped: Value = serde_json::from_slice(&stopped.stdout).unwrap();
        let operation = stopped["result"]["id"].as_str().unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        let mut startup_wait_observed = false;
        loop {
            let all = foreign_api(&f, &["operations"]);
            assert!(all.status.success());
            let all: Value = serde_json::from_slice(&all.stdout).unwrap();
            let job = all["result"]
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["id"] == operation)
                .unwrap();
            if job["phase"] == "waiting_for_provider_ready" {
                startup_wait_observed = true;
                assert_eq!(
                    unsafe { libc::kill(agent as i32, 0) },
                    0,
                    "startup waiting must retain the exact client"
                );
                let state = if account == "personal" { &f.a } else { &f.b };
                state
                    .lock()
                    .unwrap()
                    .threads
                    .get_mut(receipt["conversation_id"].as_str().unwrap())
                    .unwrap()["status"]["type"] = json!("idle");
            }
            if job["phase"] == "complete" {
                break;
            }
            assert!(job["phase"] != "failed" && Instant::now() < until, "{job}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            startup_wait_observed,
            "provider startup must be visibly pending"
        );
        assert_ne!(unsafe { libc::kill(agent as i32, 0) }, 0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let stopped_receipt: Value = serde_json::from_slice(
            &api(
                &f,
                &["resume-launch", "status", "--request-id", request_id],
                None,
            )
            .stdout,
        )
        .unwrap();
        let state: Value = serde_json::from_slice(&api(&f, &["state"], None).stdout).unwrap();
        let saved = state["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap();
        let details: Value = serde_json::from_slice(
            &api(
                &f,
                &[
                    "session",
                    "--session",
                    id,
                    "--expected-generation",
                    saved["generation"].as_str().unwrap(),
                ],
                None,
            )
            .stdout,
        )
        .unwrap();
        assert_eq!(
            details["result"]["directory"],
            json!(destination),
            "chosen directory survives exit and rediscovery"
        );
        assert_eq!(
            stopped_receipt["result"]["phase"], "running",
            "completed handoff is historical evidence, not current liveness"
        );
        assert_eq!(
            stopped_receipt["result"]["updated_at"],
            resumed["updated_at"]
        );
        assert_eq!(
            host.call(&["pane", "process-info", "--pane", origin_pane])["process_info"]["shell_pid"],
            origin_before["process_info"]["shell_pid"]
        );
    }
    for server in [&f.a, &f.b] {
        assert!(
            !server
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "turn/start" || m == "turn/steer")
        );
    }
    println!(
        "installed HERDR: exact workspace/new tab/account/UUID/socket, no duplicate launch, origin unchanged, retained shell; model turns=0"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated HERDR binary; see docs/operations.md"]
async fn installed_herdr_resumes_preserved_paginated_archive_after_account_deletion() {
    assert_eq!(
        std::env::var("SESSIONSATION_ISOLATED_HOST_TEST").as_deref(),
        Ok("1")
    );
    let f = Fixture::new().await;
    let home = f.paths.account("personal").unwrap();
    let original = history(&home, f.tmp.path(), ID);
    f.a.lock()
        .unwrap()
        .threads
        .insert(ID.into(), thread_value(&original, ID, "notLoaded"));
    paginated(&f, ID);
    let bytes = fs::read(&original).unwrap();
    let archived = home
        .join("archived_sessions")
        .join(original.file_name().unwrap());
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(&original, &archived).unwrap();
    f.a.lock().unwrap().threads.get_mut(ID).unwrap()["path"] = json!(archived);
    f.a.lock().unwrap().load_during_history = true;
    // Give the discovered archive a stable identity before its account is deleted.
    let before: Value = serde_json::from_slice(&api(&f, &["state"], None).stdout).unwrap();
    // Archived entries are normally a separate inventory. Deletion makes them
    // discoverable as retained saved sessions under the same conversation identity.
    let deletion = uuid::Uuid::new_v4().to_string();
    let admitted = api(
        &f,
        &[
            "account",
            "delete",
            "codex:personal",
            "--confirm",
            "codex:personal",
            "--request-id",
            &deletion,
        ],
        None,
    );
    assert!(
        admitted.status.success(),
        "{}",
        String::from_utf8_lossy(&admitted.stderr)
    );
    // Simulate a conversation becoming loaded during slow native pagination.
    // The fresh pre-stop check must retain its metadata server and credentials.
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let r = sessionsation::account_deletion::status(&f.paths, &deletion).unwrap();
        if r.phase == "failed" {
            assert!(
                r.detail.contains("ACCOUNT_HAS_LOADED_CONVERSATIONS"),
                "{}",
                r.detail
            );
            break;
        }
        assert!(Instant::now() < until, "{}", r.phase);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(home.join("auth.json").exists());
    assert!(sessionsation::control::available(&home));
    {
        let mut a = f.a.lock().unwrap();
        a.load_during_history = false;
        a.threads.get_mut(ID).unwrap()["status"]["type"] = json!("notLoaded");
    }
    let lock = sessionsation::paths::lock_file(
        &f.paths
            .root
            .join(sessionsation::layout::ACCOUNT_DELETIONS)
            .join(format!("{deletion}.worker.lock")),
    )
    .unwrap();
    for _ in 0..100 {
        if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
            fs2::FileExt::unlock(&lock).unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let recovered = api(&f, &["recover", &deletion], None);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let until = Instant::now() + Duration::from_secs(15);
    let receipt = loop {
        let r = sessionsation::account_deletion::status(&f.paths, &deletion).unwrap();
        if r.phase == "complete" {
            break r;
        }
        assert!(
            r.phase != "failed" && Instant::now() < until,
            "{}: {}",
            r.phase,
            r.detail
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(!home.exists());
    assert!(!receipt.history_home.join("auth.json").exists());
    let host = ActualHost::start(&f).await;
    let socket = host.config.join("herdr/sessions/fixture/herdr.sock");
    let created = host.call(&[
        "workspace",
        "create",
        "--label",
        "Retained",
        "--cwd",
        f.tmp.path().to_str().unwrap(),
    ]);
    let workspace = created["workspace"]["workspace_id"].as_str().unwrap();
    let state: Value = serde_json::from_slice(&api(&f, &["state"], None).stdout).unwrap();
    let saved = state["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == ID)
        .unwrap();
    if let Some(old) = before["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == ID)
    {
        assert_eq!(saved["id"], old["id"]);
    }
    let request = uuid::Uuid::new_v4().to_string();
    let resume = json!({"version":1,"request_id":request,"session_id":saved["id"],"expected_generation":saved["generation"],"account_id":"codex:work","name":"Archive kept","host":{"session":"fixture","socket":socket},"space":{"kind":"existing","id":workspace}});
    let out = api(&f, &["resume-launch", "start"], Some(&resume));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let until = Instant::now() + Duration::from_secs(30);
    let running = loop {
        let out: Value = serde_json::from_slice(
            &api(
                &f,
                &["resume-launch", "status", "--request-id", &request],
                None,
            )
            .stdout,
        )
        .unwrap();
        let r = &out["result"];
        if r["phase"] == "running" {
            break r.clone();
        }
        assert!(
            !matches!(r["phase"].as_str(), Some("failed" | "uncertain")) && Instant::now() < until,
            "{r}"
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
    };
    assert_eq!(
        fs::read(find_rollout(&f.paths.account("work").unwrap(), ID).unwrap()).unwrap(),
        bytes
    );
    assert_eq!(
        fs::read(
            receipt
                .history_home
                .join("archived_sessions")
                .join(archived.file_name().unwrap())
        )
        .unwrap(),
        bytes
    );
    let jobs = f.invoke(&["moves"]);
    let job = jobs
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == request)
        .unwrap();
    assert_eq!(job["history_checks"][0]["turns"], 70);
    assert_eq!(job["history_checks"][0]["items"], 270);
    assert_eq!(running["conversation_id"], ID);
    for server in [&f.a, &f.b] {
        assert!(
            !server
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "turn/start" || m == "turn/steer")
        );
    }
    host.call(&["pane", "run", running["pane_id"].as_str().unwrap(), "/exit"]);
    println!(
        "isolated HERDR: local credentials deleted; archived paginated conversation resumed on explicit destination with matching 70 turns/270 items; model turns=0"
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
[ "$HERDR_SOCKET_PATH" = "$FIXTURE_DIR/host.sock" ] || exit 41
[ "$HERDR_SESSION" = fixture ] || exit 41
[ "$1" != --session ] || exit 41
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
    // No launch_permissions configured: the yolo default is verified at
    // creation and persisted before the plain remote TUI attaches, so a reload
    // cannot fall back to config.toml or folder-trust defaults.
    let conversation = receipt["conversation_id"].as_str().unwrap();
    {
        let server = f.a.lock().unwrap();
        let start = &server
            .calls
            .iter()
            .find(|(m, _)| m == "thread/start")
            .unwrap()
            .1;
        assert_eq!(start["approvalPolicy"], "never");
        assert_eq!(start["permissions"], ":danger-full-access");
        assert_eq!(
            persisted(&server, conversation),
            yolo_settings(conversation)
        );
    }
    let home = f.paths.account("personal").unwrap();
    wait_until(|| home.join("launch-args").exists());
    assert_plain_remote_tui(&home, conversation);
    let session = sessionsation::catalog::list(&f.paths)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap();
    assert_eq!(session.name, "Popup name");
    assert_eq!(session.account, "personal");
    // A worker timeout can precede provider exec. Reconcile evidence only using
    // the original request, without repeating native or terminal dispatch.
    let receipt_path = f
        .paths
        .root
        .join(sessionsation::layout::LAUNCHES)
        .join(format!("{id}.json"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated HERDR binary; see docs/operations.md"]
async fn installed_herdr_explicit_message_recovery_keeps_context_and_never_replays() {
    assert_eq!(
        std::env::var("SESSIONSATION_ISOLATED_HOST_TEST").as_deref(),
        Ok("1")
    );
    for mode in ["stopped", "interrupted", "errored"] {
        let f = Fixture::new().await;
        let host = ActualHost::start(&f).await;
        let socket = host.config.join("herdr/sessions/fixture/herdr.sock");
        let origin = host.call(&[
            "workspace",
            "create",
            "--label",
            "Unrelated shell",
            "--cwd",
            f.tmp.path().to_str().unwrap(),
        ]);
        let workspace = origin["workspace"]["workspace_id"].as_str().unwrap();
        let pane = origin["root_pane"]["pane_id"].as_str().unwrap();
        let before = host.call(&["pane", "process-info", "--pane", pane]);
        let s = super::recovery::saved(
            &f,
            match mode {
                "interrupted" => "interrupted",
                "errored" => "failed",
                _ => "completed",
            },
        );
        let history = find_rollout(&f.paths.account("personal").unwrap(), ID).unwrap();
        let original_history = fs::read(&history).unwrap();
        let input = json!({"session_id":s["id"],"expected_generation":s["generation"],"host":{"kind":"herdr","session":"fixture","socket":socket,"workspace_id":workspace}});
        let p = api(&f, &["recovery-plan"], Some(&input));
        assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        let plan = serde_json::from_slice::<Value>(&p.stdout).unwrap()["result"].clone();
        let req = super::recovery::request(&s, mode, None, Some(plan));
        let send = api(&f, &["send"], Some(&req));
        assert!(
            send.status.success(),
            "{}",
            String::from_utf8_lossy(&send.stderr)
        );
        let done = super::recovery::receipt(&f, req["message_id"].as_str().unwrap()).await;
        assert_eq!(done["launch"]["phase"], "complete", "{done}");
        assert_eq!(done["launch"]["confirmed"], true, "{done}");
        assert_eq!(done["facts"]["accepted_native"], true, "{done}");
        assert_eq!(done["session_id"], s["id"]);
        assert_eq!(done["provider_conversation_id"], ID);
        assert_eq!(
            host.call(&["pane", "process-info", "--pane", pane])["process_info"]["shell_pid"],
            before["process_info"]["shell_pid"]
        );
        assert_eq!(fs::read(&history).unwrap(), original_history);
        assert!(api(&f, &["send"], Some(&req)).status.success());
        let calls = f.a.lock().unwrap().calls.clone();
        assert_eq!(
            calls.iter().filter(|(m, _)| m == "thread/resume").count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(m, _)| m == "thread/queue/add")
                .count(),
            1
        );
        assert!(
            !calls
                .iter()
                .any(|(m, _)| matches!(m.as_str(), "thread/start" | "turn/start" | "turn/steer"))
        );
        eprintln!(
            "owned HERDR explicit {mode} recovery: same backend/conversation/account/directory/history; queue attempts=1, unrelated shell unchanged, models=0"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated HERDR binary; see docs/operations.md"]
async fn installed_herdr_recovers_idle_error_history_writer_without_external_help() {
    assert_eq!(
        std::env::var("SESSIONSATION_ISOLATED_HOST_TEST").as_deref(),
        Ok("1")
    );
    for account in ["personal", "work"] {
        let f = Fixture::new().await;
        let host = ActualHost::start(&f).await;
        let socket = host.config.join("herdr/sessions/fixture/herdr.sock");
        let space = host.call(&[
            "workspace",
            "create",
            "--label",
            "Recovery",
            "--cwd",
            f.tmp.path().to_str().unwrap(),
        ]);
        let path = history(&f.paths.account("personal").unwrap(), f.tmp.path(), ID);
        {
            let mut server = f.a.lock().unwrap();
            server.retained_writer = Some(fs::OpenOptions::new().append(true).open(&path).unwrap());
            server.reject_error_full_read = true;
            let mut t = thread_value(&path, ID, "systemError");
            t["turns"] = json!([{"id":"failure","status":"failed"}]);
            server.threads.insert(ID.into(), t);
        }
        let out = api(&f, &["state"], None);
        assert!(out.status.success());
        let state: Value = serde_json::from_slice(&out.stdout).unwrap();
        let saved = state["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["conversation_id"] == ID)
            .unwrap();
        assert_eq!(saved["resume_block_reason"], "unfinished_history");
        assert_eq!(saved["capabilities"]["resume"], true);
        let request = uuid::Uuid::new_v4().to_string();
        let resume = json!({"version":1,"request_id":request,"session_id":saved["id"],"expected_generation":saved["generation"],"account_id":format!("codex:{account}"),"name":"Recovered","host":{"session":"fixture","socket":socket},"space":{"kind":"existing","id":space["workspace"]["workspace_id"]},"confirm_unfinished":true});
        let out = api(&f, &["resume-launch", "start"], Some(&resume));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let until = Instant::now() + Duration::from_secs(25);
        let receipt = loop {
            let out = api(
                &f,
                &["resume-launch", "status", "--request-id", &request],
                None,
            );
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let result: Value = serde_json::from_slice(&out.stdout).unwrap();
            let r = &result["result"];
            if r["phase"] == "running" {
                break r.clone();
            }
            assert!(
                !matches!(r["phase"].as_str(), Some("failed" | "uncertain"))
                    && Instant::now() < until,
                "{r}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(receipt["conversation_id"], ID);
        assert_eq!(receipt["request"]["session_id"], saved["id"]);
        let pane = receipt["pane_id"].as_str().unwrap();
        let runtime = host.call(&["pane", "process-info", "--pane", pane]);
        assert!(
            runtime["process_info"]["foreground_processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == "codex")
        );
        assert!(
            !f.a.lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "turn/start" || m == "turn/steer")
        );
        assert!(
            !f.b.lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "turn/start" || m == "turn/steer")
        );
    }
}

async fn wait_space_move(f: &Fixture, id: &str, phase: &str) -> Value {
    let until = Instant::now() + Duration::from_secs(20);
    let path = f
        .paths
        .root
        .join(sessionsation::layout::SPACE_MOVES)
        .join(format!("{id}.json"));
    loop {
        let op: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .unwrap();
        if op["phase"] == phase && fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
            return op;
        }
        assert!(Instant::now() < until, "Expected {phase}: {op}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Moving into a new Space creates it around the moved pane: no initial shell
/// pane exists, a retry never creates a second Space, and an existing label is
/// refused before anything changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated HERDR binary; see docs/operations.md"]
async fn installed_herdr_space_move_creates_new_space_without_stray_shell_or_replay() {
    assert_eq!(
        std::env::var("SESSIONSATION_ISOLATED_HOST_TEST").as_deref(),
        Ok("1")
    );
    let f = Fixture::new().await;
    let host = ActualHost::start(&f).await;
    let socket = host.config.join("herdr/sessions/fixture/herdr.sock");
    let origin = host.call(&[
        "workspace",
        "create",
        "--label",
        "Origin",
        "--cwd",
        f.tmp.path().to_str().unwrap(),
    ]);
    let w1 = origin["workspace"]["workspace_id"].as_str().unwrap();
    let mut r = request(&f, &socket, "New space fixture");
    r["space"]["id"] = json!(w1);
    let id = r["request_id"].as_str().unwrap();
    assert!(api(&f, &["launch", "start"], Some(&r)).status.success());
    let until = Instant::now() + Duration::from_secs(40);
    let receipt = loop {
        let out = api(&f, &["launch", "status", "--request-id", id], None);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["result"]["phase"] == "running" {
            break v["result"].clone();
        }
        assert!(Instant::now() < until, "{v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let pane = receipt["pane_id"].as_str().unwrap();
    let terminal = host.call(&["pane", "get", pane])["pane"]["terminal_id"].clone();
    // The source tab keeps a sibling so the move cannot empty the Origin Space.
    host.call(&["pane", "split", pane, "--direction", "right", "--no-focus"]);
    let sessions: Value = serde_json::from_slice(&foreign_api(&f, &["state"]).stdout).unwrap();
    let session = sessions["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap()
        .clone();
    let generation = session["generation"].as_str().unwrap();
    let spaces_before = host.call(&["workspace", "list"])["workspaces"]
        .as_array()
        .unwrap()
        .len();
    let panes_before = host.call(&["pane", "list"])["panes"]
        .as_array()
        .unwrap()
        .len();
    let move_id = uuid::Uuid::new_v4().to_string();
    let args = [
        "move-space",
        "--session",
        id,
        "--expected-generation",
        generation,
        "--new-space",
        "Fresh",
        "--request-id",
        &move_id,
    ];
    assert!(
        !foreign_api(
            &f,
            &[
                "move-space",
                "--session",
                id,
                "--expected-generation",
                generation,
                "--new-space",
                "A",
                "--space",
                w1,
                "--request-id",
                &move_id
            ]
        )
        .status
        .success(),
        "--space and --new-space are exclusive"
    );
    let taken = uuid::Uuid::new_v4().to_string();
    let refused = foreign_api(
        &f,
        &[
            "move-space",
            "--session",
            id,
            "--expected-generation",
            generation,
            "--new-space",
            "Origin",
            "--request-id",
            &taken,
        ],
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("WORKSPACE_NAME_EXISTS"));
    assert!(
        !f.paths
            .root
            .join(sessionsation::layout::SPACE_MOVES)
            .join(format!("{taken}.json"))
            .exists()
    );
    let started = foreign_api(&f, &args);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let done = wait_space_move(&f, &move_id, "complete").await;
    let created = done["created_space"].as_str().unwrap().to_owned();
    assert_eq!(done["pane"]["terminal_id"], terminal);
    let list = host.call(&["workspace", "list"]);
    let list = list["workspaces"].as_array().unwrap();
    assert_eq!(list.len(), spaces_before + 1);
    let fresh = list
        .iter()
        .find(|w| w["workspace_id"] == created.as_str())
        .unwrap();
    assert_eq!(fresh["label"], "Fresh");
    assert_eq!(
        (fresh["pane_count"].as_u64(), fresh["tab_count"].as_u64()),
        (Some(1), Some(1)),
        "no stray shell: {fresh}"
    );
    assert_eq!(
        host.call(&["pane", "list"])["panes"]
            .as_array()
            .unwrap()
            .len(),
        panes_before
    );
    let repeat: Value = serde_json::from_slice(&foreign_api(&f, &args).stdout).unwrap();
    assert_eq!(repeat["result"]["created_space_id"], created.as_str());
    assert_eq!(repeat["result"]["space_id"], created.as_str());
    assert_eq!(
        host.call(&["workspace", "list"])["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        spaces_before + 1
    );
}
