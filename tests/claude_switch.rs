//! Claude account switch (contract 1.34.0) with a metadata-only fake (`support/fake-claude.sh`)
//! and a fake HERDR. Disposable homes only; no provider network, credentials or model requests.
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
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
struct Fixture {
    _tmp: tempfile::TempDir,
    paths: Paths,
    project: PathBuf,
    _daemon: Process,
}
impl Fixture {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(binary());
        c.arg("--data-dir")
            .arg(&self.paths.root)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("HERDR_ENV")
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdin(Stdio::null());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn api(&self, args: &[&str]) -> Value {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    }
    fn claude_sessions(&self) -> Vec<Value> {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["provider"] == "claude")
            .cloned()
            .collect()
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("accounts/claude").join(name)
    }
}
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[allow(dead_code)]
fn kill(mut child: Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    child.wait().unwrap();
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let install = |name: &str, body: &str| {
        let path = tmp.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
    let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
        ..Default::default()
    };
    sessionsation::store::save_config(&paths, &config).unwrap();
    let daemon = Process(
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon::call(&paths, "status", json!({})).await.is_err() {
        assert!(
            Instant::now() < deadline,
            "fixture supervisor did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = std::fs::canonicalize(project).unwrap();
    Fixture {
        _tmp: tmp,
        paths,
        project,
        _daemon: daemon,
    }
}
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
impl Fixture {
    fn add_logged_in(&self, name: &str) {
        let out = self.run(&["account", "add", "--platform", "claude", name]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let id = format!("claude:{name}");
        wait_for("auth status poll", || {
            self.api(&["account", "list"])
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == id.as_str() && a["auth_required"] == false)
        });
    }
    /// A stopped session discovered from a transcript in the account home.
    fn stopped_session(&self, account: &str) -> (String, String) {
        let native = uuid::Uuid::new_v4().to_string();
        let slug = self.project.to_string_lossy().replace('/', "-");
        let dir = self.home(account).join("projects").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{native}.jsonl")),
            format!(
                "{}\n",
                json!({"type":"user","sessionId":native,"cwd":self.project,"timestamp":"2026-01-01T00:00:00Z"})
            ),
        )
        .unwrap();
        let s = self
            .claude_sessions()
            .into_iter()
            .find(|s| s["conversation_id"] == native.as_str())
            .unwrap();
        assert_eq!(s["availability"], "stopped");
        (s["id"].as_str().unwrap().to_owned(), native)
    }
}

struct Setup {
    f: Fixture,
    path: String,
    herdr_state: PathBuf,
}
impl Setup {
    /// Two logged-in accounts and a fake HERDR on PATH for every command.
    async fn new() -> Self {
        let f = fixture().await;
        f.add_logged_in("a");
        f.add_logged_in("b");
        let tmp = f.paths.root.parent().unwrap().to_path_buf();
        let bin = tmp.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let herdr = bin.join("herdr");
        std::fs::write(&herdr, include_str!("support/fake-herdr.sh")).unwrap();
        std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let herdr_state = tmp.join("herdr-state");
        Self {
            f,
            path,
            herdr_state,
        }
    }
    fn output(&self, args: &[&str]) -> Output {
        self.f
            .command(args)
            .env("PATH", &self.path)
            .env("FAKE_HERDR_DIR", &self.herdr_state)
            .output()
            .unwrap()
    }
    fn api(&self, args: &[&str]) -> Value {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.output(&all);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    }
    fn api_error(&self, args: &[&str]) -> String {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.output(&all);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn session(&self, id: &str) -> Value {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    }
    fn switch(&self, id: &str, to: &str, extra: &[&str]) -> Result<Value, String> {
        let s = self.session(id);
        let mut args = vec![
            "api",
            "switch",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--to",
            to,
        ];
        args.extend_from_slice(extra);
        let out = self.output(&args);
        if out.status.success() {
            Ok(serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }
    fn switches(&self) -> Vec<Value> {
        self.api(&["operations"])
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["provider"] == "claude" && o["kind"] == "account")
            .cloned()
            .collect()
    }
    fn operation(&self, id: &str) -> Value {
        let mut last = Value::Null;
        wait_for("switch operation", || {
            last = self
                .api(&["operations"])
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["id"] == id)
                .cloned()
                .unwrap_or(Value::Null);
            matches!(
                last["phase"].as_str(),
                Some("complete" | "failed" | "uncertain" | "abandoned")
            )
        });
        // The worker must have released its lock before another action starts.
        let lock = self
            .f
            .paths
            .root
            .join(sessionsation::layout::CLAUDE_SWITCHES)
            .join(format!("{id}.worker.lock"));
        wait_for("worker exit", || {
            let f = std::fs::OpenOptions::new().write(true).open(&lock);
            f.is_err() || fs2::FileExt::try_lock_exclusive(&f.unwrap()).is_ok()
        });
        last
    }
}

/// Sidecars of one conversation in a home, plus account memory that must stay.
fn populate(home: &Path, project: &Path, native: &str) {
    let slug = project.to_string_lossy().replace('/', "-");
    let write = |rel: String, text: &str| {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write(
        format!("projects/{slug}/{native}/tool-results/r.txt"),
        "result",
    );
    write(format!("file-history/{native}/h1"), "history");
    write(format!("session-env/{native}/e1"), "env");
    write(format!("todos/{native}-agent-1.json"), "[]");
    write(
        format!("projects/{slug}/memory/MEMORY.md"),
        "account memory",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertised_capability_and_a_stopped_session_switches_with_memory_left_behind() {
    let t = Setup::new().await;
    let caps = t.api(&["capabilities"]);
    let claude = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "claude")
        .unwrap();
    assert_eq!(claude["account_switch"], true);
    assert_eq!(claude["operations"]["account_switch"]["supported"], true);
    assert_eq!(caps["contract_version"], "1.42.0");

    let (id, native) = t.f.stopped_session("a");
    populate(&t.f.home("a"), &t.f.project, &native);
    let before = t.session(&id);
    assert_eq!(before["capabilities"]["switch_account"], true, "{before}");
    assert!(before["capability_reasons"]["switch_account"].is_null());
    assert_eq!(before["account_id"], "claude:a");
    let memory = t.f.home("a").join(format!(
        "projects/{}/memory/MEMORY.md",
        t.f.project.to_string_lossy().replace('/', "-")
    ));

    // Admission refusals change nothing.
    let stale = t.api_error(&[
        "switch",
        "--session",
        &id,
        "--expected-generation",
        "0000",
        "--to",
        "claude:b",
    ]);
    assert!(stale.contains("STALE_TARGET"), "{stale}");
    assert!(
        t.switch(&id, "claude:a", &[])
            .unwrap_err()
            .contains("SWITCH_SAME_ACCOUNT")
    );
    assert!(
        t.switch(&id, "codex:b", &[])
            .unwrap_err()
            .contains("ACCOUNT_PROVIDER_REQUIRED")
    );
    assert!(
        t.switch(&id, "b", &[])
            .unwrap_err()
            .contains("ACCOUNT_PROVIDER_REQUIRED")
    );
    assert!(
        t.switch(&id, "claude:missing", &[])
            .unwrap_err()
            .contains("ACCOUNT_UNAVAILABLE")
    );
    assert!(
        t.f.home("a")
            .join(format!(
                "projects/{}/{native}.jsonl",
                t.f.project.to_string_lossy().replace('/', "-")
            ))
            .exists()
    );

    // A conflicting copy in the target refuses before anything moves.
    let slug = t.f.project.to_string_lossy().replace('/', "-");
    let conflict = t.f.home("b").join(format!("file-history/{native}/theirs"));
    std::fs::create_dir_all(conflict.parent().unwrap()).unwrap();
    std::fs::write(&conflict, "theirs").unwrap();
    let refused = t.switch(&id, "claude:b", &[]).unwrap_err();
    assert!(refused.contains("TARGET_CONFLICT"), "{refused}");
    assert!(t.switches().is_empty());
    std::fs::remove_dir_all(conflict.parent().unwrap()).unwrap();

    let op = t.switch(&id, "claude:b", &[]).unwrap();
    assert_eq!(op["kind"], "account");
    assert_eq!(op["provider"], "claude");
    assert_eq!(op["source_account_id"], "claude:a");
    assert_eq!(op["target_account_id"], "claude:b");
    let done = t.operation(op["id"].as_str().unwrap());
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["resume"]["state"], "not_needed");
    assert_eq!(done["receipt"]["account_memory"]["moved"], false);
    assert_eq!(done["receipt"]["account_memory"]["stays_with"], "claude:a");
    assert_eq!(done["receipt"]["files_moved"], 5);

    let after = t.session(&id);
    assert_eq!(after["account_id"], "claude:b");
    assert_eq!(after["availability"], "stopped");
    assert_eq!(after["generation"], done["generation"]);
    assert_ne!(after["generation"], before["generation"]);
    assert_eq!(after["conversation_id"], native.as_str());
    let (a, b) = (t.f.home("a"), t.f.home("b"));
    for rel in [
        format!("projects/{slug}/{native}.jsonl"),
        format!("projects/{slug}/{native}/tool-results/r.txt"),
        format!("file-history/{native}/h1"),
        format!("session-env/{native}/e1"),
        format!("todos/{native}-agent-1.json"),
    ] {
        assert!(b.join(&rel).exists(), "{rel} missing in b");
        assert!(!a.join(&rel).exists(), "{rel} left in a");
    }
    assert_eq!(read(&memory), "account memory", "memory untouched");
    assert!(
        !b.join(format!("projects/{slug}/memory")).exists(),
        "memory must not move"
    );

    // The moved conversation is still addressable by its stable id and resumes in b.
    let again = t.switch(&id, "claude:a", &[]).unwrap();
    let back = t.operation(again["id"].as_str().unwrap());
    assert_eq!(back["phase"], "complete", "{back}");
    assert_eq!(t.session(&id)["account_id"], "claude:a");
    assert_eq!(read(&memory), "account memory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_sessions_are_refused_with_a_clear_code() {
    let t = Setup::new().await;
    let (id, native) = t.f.stopped_session("a");
    // A hosted (stream-json) host record that is still starting counts as a live writer.
    let host =
        t.f.paths
            .root
            .join(sessionsation::layout::CLAUDE_HOSTED)
            .join(&native);
    std::fs::create_dir_all(&host).unwrap();
    let now = daemon::now();
    std::fs::write(
        host.join("host.json"),
        json!({"schema_version":1,"conversation_id":native,"session_id":native,"account":"a",
            "directory":t.f.project,"origin":"resume","permission_mode":"account",
            "effective_permission_mode":null,"epoch":1,"state":"starting","reason":null,
            "worker_pid":null,"worker_start":null,"child_pid":null,"child_start":null,"busy":false,
            "pending_permissions":[],"created_at":now,"updated_at":now,"started_at":null,"stopped_at":null})
        .to_string(),
    )
    .unwrap();
    let error = t.switch(&id, "claude:b", &[]).unwrap_err();
    assert!(error.contains("SESSION_HOSTED"), "{error}");
    assert!(t.switches().is_empty());
    assert_eq!(t.session(&id)["account_id"], "claude:a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_session_is_stopped_moved_and_resumed_in_its_pane_with_recorded_permissions() {
    let t = Setup::new().await;
    let (id, native) = t.f.stopped_session("a");
    populate(&t.f.home("a"), &t.f.project, &native);
    let s = t.session(&id);
    // Start it in a (fake) HERDR pane with explicit non-bypass permissions.
    let out = t.output(&[
        "--permissions",
        "default",
        "api",
        "resume",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let running = t.session(&id);
    assert_eq!(running["availability"], "running", "{running}");
    assert_eq!(running["attachment"]["target"], "herdr/fixture/w1:p2");
    assert_eq!(running["capabilities"]["switch_account"], true, "{running}");
    let old_pid = running["pid"].as_u64().unwrap() as i32;
    assert!(read(&t.f.home("a").join("fake-launch-resume")).contains("bypass=\n"));

    // The fake process reports status busy: stopping it needs explicit consent.
    let refused = t.switch(&id, "claude:b", &[]).unwrap_err();
    assert!(refused.contains("RISK_CONFIRMATION_REQUIRED"), "{refused}");
    assert_eq!(t.session(&id)["availability"], "running");
    assert!(t.switches().is_empty());

    let op = t.switch(&id, "claude:b", &["--accept-risks"]).unwrap();
    let done = t.operation(op["id"].as_str().unwrap());
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["resume"]["state"], "confirmed");
    assert_eq!(done["resume"]["pane"], "herdr/fixture/w1:p2");
    assert_eq!(done["resume"]["permissions"], "default");

    // The old process is gone; the conversation runs from b in the same pane.
    wait_for("old process exit", || {
        std::fs::read_to_string(format!("/proc/{old_pid}/stat"))
            .map_or(true, |stat| stat.contains(") Z "))
    });
    let moved = t.session(&id);
    assert_eq!(moved["account_id"], "claude:b");
    assert_eq!(moved["availability"], "running", "{moved}");
    assert_eq!(moved["attachment"]["target"], "herdr/fixture/w1:p2");
    assert_ne!(moved["pid"].as_u64().unwrap() as i32, old_pid);
    let launch = read(&t.f.home("b").join("fake-launch-resume"));
    assert!(launch.contains(&format!("session={native}")), "{launch}");
    assert!(
        launch.contains(&format!("config_dir={}", t.f.home("b").display())),
        "{launch}"
    );
    assert!(
        launch.contains("bypass=\n"),
        "recorded permissions preserved: {launch}"
    );
    assert!(
        launch.contains(&format!("cwd={}", t.f.project.display())),
        "{launch}"
    );
    let slug = t.f.project.to_string_lossy().replace('/', "-");
    assert!(
        !t.f.home("a")
            .join(format!("projects/{slug}/{native}.jsonl"))
            .exists()
    );
    assert!(
        t.f.home("b")
            .join(format!("projects/{slug}/{native}.jsonl"))
            .exists()
    );
    assert_eq!(
        read(
            &t.f.home("a")
                .join(format!("projects/{slug}/memory/MEMORY.md"))
        ),
        "account memory"
    );
    assert_eq!(
        read(&t.herdr_state.join("calls"))
            .matches("pane run")
            .count(),
        2
    );
    if let Ok(pid) = read(&t.herdr_state.join("pid-w1:p2")).trim().parse::<i32>() {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    if let Some(pid) = moved["pid"].as_u64() {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_session_outside_a_recorded_pane_is_refused_and_left_alone() {
    let t = Setup::new().await;
    // An interactive session in a terminal Sessionsation does not know (no pane recorded).
    let mut child =
        t.f.command(&["run", "--account", "claude:a"])
            .current_dir(&t.f.project)
            .spawn()
            .unwrap();
    wait_for("interactive launch", || {
        t.f.home("a").join("fake-launch-new").exists()
    });
    let native = read(&t.f.home("a").join("fake-launch-new"))
        .lines()
        .find_map(|l| l.strip_prefix("session="))
        .unwrap()
        .to_owned();
    let mut id = String::new();
    wait_for("running", || {
        let found = t.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["conversation_id"] == native.as_str() && s["availability"] == "running")
            .map(|s| s["id"].as_str().unwrap().to_owned());
        id = found.clone().unwrap_or_default();
        found.is_some()
    });
    let running = t.session(&id);
    assert_eq!(running["capabilities"]["switch_account"], false);
    let reason = running["capability_reasons"]["switch_account"].to_string();
    assert!(
        reason.starts_with("\"SESSION_PANE_UNKNOWN"),
        "{reason} in {running}"
    );
    let refused = t.switch(&id, "claude:b", &["--accept-risks"]).unwrap_err();
    assert!(refused.contains("SESSION_PANE_UNKNOWN"), "{refused}");
    assert_eq!(t.session(&id)["availability"], "running");
    assert!(
        child.try_wait().unwrap().is_none(),
        "the process is untouched"
    );
    let pid = running["pid"].as_u64().unwrap() as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = child.wait();
}
