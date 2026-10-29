//! Antigravity CLI (`agy`) provider integration with a metadata-only fake
//! (`support/fake-agy.sh`, installed as `agy`). Disposable homes only; no
//! provider network, credentials or model requests.
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
    fake: PathBuf,
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
            .env_remove("AGY_ACCOUNT_HOME")
            .env("GEMINI_API_KEY", "key-must-not-reach-agy")
            .env("XDG_RUNTIME_DIR", "/run/user/fixture")
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
    fn api_error(&self, args: &[&str]) -> String {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn sessions(&self) -> Vec<Value> {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["provider"] == "antigravity")
            .cloned()
            .collect()
    }
    fn by_conversation(&self, native: &str) -> Value {
        self.sessions()
            .into_iter()
            .find(|s| s["conversation_id"] == native)
            .unwrap_or_else(|| panic!("no antigravity session for {native}"))
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("antigravity-accounts").join(name)
    }
    fn send(&self, body: &Value) -> Output {
        let mut child = self
            .command(&["api", "send"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
    fn send_ok(&self, body: &Value) -> Value {
        let out = self.send(body);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    }
    fn send_err(&self, body: &Value) -> String {
        let out = self.send(body);
        assert!(!out.status.success(), "unexpectedly accepted {body}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn settled(&self, message: &str) -> Value {
        let mut last = Value::Null;
        wait_for("headless receipt", || {
            last = self.api(&["receipt", message]);
            !matches!(
                last["state"].as_str(),
                Some("pending_local" | "execution_started")
            )
        });
        last
    }
}
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("agy");
    std::fs::write(&fake, include_str!("support/fake-agy.sh")).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        antigravity_binary: fake.to_str().unwrap().into(),
        codex_binary: "/nonexistent/codex".into(),
        claude_binary: "/nonexistent/claude".into(),
        gemini_binary: "/nonexistent/gemini".into(),
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
        fake,
        _daemon: daemon,
    }
}
fn new_body(id: &str, account: &str, dir: &Path, text: &str) -> Value {
    json!({"api_version":1,"message_id":id,"mode":"new","account_id":account,
        "directory":dir,"text":text,"expires_at":now()+600})
}
fn resume_body(id: &str, session: &Value, text: &str) -> Value {
    json!({"api_version":1,"message_id":id,"mode":"resume","session_id":session["id"],
        "expected_generation":session["generation"],"text":text,"expires_at":now()+600})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accounts_enroll_log_in_and_report_unsupported_usage_logout_and_limits() {
    let f = fixture().await;
    let added = f.api(&["account", "add", "antigravity:fresh"]);
    assert_eq!(added["provider"], "antigravity");
    assert_eq!(added["auth"]["problem"], "AUTH_REQUIRED");
    assert_eq!(added["login_required"], true);
    let argv: Vec<String> = serde_json::from_value(added["login"]["argv"].clone()).unwrap();
    assert_eq!(argv.last().unwrap(), "antigravity:fresh");
    let home = f.home("fresh");
    assert_eq!(
        std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // Names stay global across providers.
    let taken = f.api_error(&["account", "add", "gemini:fresh"]);
    assert!(
        taken.contains("ACCOUNT_NAME_EXISTS") || taken.contains("ACCOUNT_PROVIDER_MISMATCH"),
        "{taken}"
    );
    // Unauthenticated use never reaches agy (no OAuth wait).
    let refused = f.send_err(&new_body(
        &uuid::Uuid::new_v4().to_string(),
        "antigravity:fresh",
        &f.project,
        "hello",
    ));
    assert!(refused.contains("AUTH_REQUIRED"), "{refused}");
    assert!(!home.join("fake-agy-calls").exists(), "agy was started");
    // The login runs agy itself in the account environment (fake leaves a token).
    let out = f.run(&["account", "login", "antigravity:fresh"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = read(&home.join("fake-agy-calls"));
    assert!(calls.contains("update=1"), "{calls}");
    assert!(
        calls.contains(&format!("cache={}/.cache", home.display())),
        "{calls}"
    );
    assert!(
        calls.contains("runtime=\n") && calls.contains("api_key=\n"),
        "{calls}"
    );
    let refreshed = f.api(&["account", "refresh", "antigravity:fresh"]);
    assert_eq!(refreshed["auth_required"], false, "{refreshed}");
    assert_eq!(refreshed["auth_mode"]["auth_method"], "google-oauth");
    let row = f
        .api(&["account", "list"])
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "antigravity:fresh")
        .cloned()
        .unwrap();
    assert_eq!(row["usage_state"], "unsupported");
    assert!(row["usage"]["short"].is_null() && row["usage"]["weekly"].is_null());
    assert_eq!(row["selectable"], true);
    assert_eq!(
        row["provider_capabilities"]["account_logout"]["supported"],
        false
    );
    let caps = f.api(&["capabilities"]);
    assert_eq!(caps["contract_revision"], 24);
    let p = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "antigravity")
        .cloned()
        .unwrap();
    assert_eq!(p["implemented"], true);
    assert_eq!(p["account_logout"], false);
    assert_eq!(p["usage_state"], "unsupported");
    assert_eq!(caps["antigravity_input_delivery"]["herdr_agent"], "agy");
    assert_eq!(
        caps["launch_permissions"]["antigravity"]["yolo"],
        "--dangerously-skip-permissions"
    );
    assert!(
        f.api_error(&["account", "logout", "antigravity:fresh"])
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
    assert!(
        f.api_error(&[
            "account",
            "limits",
            "antigravity:fresh",
            "--short-used",
            "50",
            "--weekly-used",
            "50"
        ])
        .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_new_creates_the_conversation_first_and_resume_delivers_once() {
    let f = fixture().await;
    f.api(&["account", "add", "antigravity:work"]);
    assert!(
        f.run(&["account", "login", "antigravity:work"])
            .status
            .success()
    );
    f.api(&["account", "refresh", "antigravity:work"]);
    let home = f.home("work");
    let id = uuid::Uuid::new_v4().to_string();
    let text = "/help is plain text here; say hi";
    let accepted = f.send_ok(&new_body(&id, "antigravity:work", &f.project, text));
    assert_eq!(accepted["provider"], "antigravity");
    let receipt = f.settled(&id);
    assert_eq!(receipt["state"], "completed_with_reply", "{receipt}");
    assert_eq!(receipt["reply"]["text"], "fake reply\n");
    assert_eq!(receipt["correlation"]["matched"], true);
    let native = receipt["provider_conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The message went on stdin as one stream-json user event, never in argv.
    let input: Value = serde_json::from_str(read(&home.join("fake-last-input")).trim()).unwrap();
    assert_eq!(input["event"], "user");
    assert_eq!(input["message"]["content"][0]["text"], text);
    let calls = read(&home.join("fake-agy-calls"));
    assert!(!calls.contains("say hi"), "{calls}");
    assert!(calls.contains("--print-timeout 595s"), "{calls}");
    assert!(calls.contains("--disable-slash-commands"), "{calls}");
    assert!(
        calls.contains(&format!("--conversation {native}")),
        "{calls}"
    );
    assert!(!calls.contains("--dangerously-skip-permissions"), "{calls}");
    // The catalog entry is the created conversation; it is stopped.
    let s = f.by_conversation(&native);
    assert_eq!(s["availability"], "stopped", "{s}");
    assert_eq!(s["directory"], f.project.to_str().unwrap());
    assert_eq!(s["capabilities"]["archive"], false);
    assert_eq!(s["capabilities"]["delete"], false);
    assert!(
        s["capability_reasons"]["archive"]
            .as_str()
            .unwrap()
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
    assert_eq!(s["capabilities"]["submit"], false);
    assert!(s["pending_questions_unsupported_reason"].is_string());
    // Resume delivers on the same conversation; a different reported id is unknown.
    let again = uuid::Uuid::new_v4().to_string();
    f.send_ok(&resume_body(&again, &s, "and again"));
    let r = f.settled(&again);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    std::fs::write(
        home.join("fake-print-session"),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    let s = f.by_conversation(&native);
    let mismatch = uuid::Uuid::new_v4().to_string();
    f.send_ok(&resume_body(&mismatch, &s, "third"));
    let r = f.settled(&mismatch);
    assert_eq!(r["state"], "delivery_unknown", "{r}");
    assert_eq!(r["reason"], "SESSION_ID_MISMATCH");
    std::fs::remove_file(home.join("fake-print-session")).unwrap();
    // A reported error is a failed delivery, not success.
    std::fs::write(home.join("fake-print-error"), "1").unwrap();
    let s = f.by_conversation(&native);
    let failed = uuid::Uuid::new_v4().to_string();
    f.send_ok(&resume_body(&failed, &s, "fourth"));
    let r = f.settled(&failed);
    assert_eq!(r["state"], "failed", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_agy_process_holding_the_conversation_is_running_and_refuses_resume() {
    let f = fixture().await;
    f.api(&["account", "add", "antigravity:live"]);
    assert!(
        f.run(&["account", "login", "antigravity:live"])
            .status
            .success()
    );
    f.api(&["account", "refresh", "antigravity:live"]);
    let home = f.home("live");
    let id = uuid::Uuid::new_v4().to_string();
    f.send_ok(&new_body(&id, "antigravity:live", &f.project, "start"));
    let native = f.settled(&id)["provider_conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // An interactive agy outside Sessionsation opens the conversation.
    let child = Process(
        Command::new(&f.fake)
            .args(["--conversation", &native])
            .env("HOME", &home)
            .env("AGY_ACCOUNT_HOME", &home)
            .current_dir(&f.project)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for("live session", || {
        f.by_conversation(&native)["availability"] == "running"
    });
    let s = f.by_conversation(&native);
    assert_eq!(s["pid"], child.0.id());
    assert_eq!(s["capabilities"]["submit"], false);
    assert!(
        s["capability_reasons"]["submit"]
            .as_str()
            .unwrap()
            .contains("TERMINAL_NOT_ATTACHED"),
        "{s}"
    );
    let busy = f.send_err(&resume_body(&uuid::Uuid::new_v4().to_string(), &s, "x"));
    assert!(busy.contains("SESSION_ALREADY_RUNNING"), "{busy}");
    drop(child);
    wait_for("stopped", || {
        f.by_conversation(&native)["availability"] == "stopped"
    });
}
