#![allow(dead_code)]
//! Inline run/resume journaling and --name (contract 1.32), driven through the
//! Gemini fake (`support/fake-gemini.sh`). Disposable homes only; no provider
//! network, credentials or model requests.
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
            .env_remove("GEMINI_CLI_HOME")
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
    fn sessions_of(&self, provider: &str) -> Vec<Value> {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["provider"] == provider)
            .cloned()
            .collect()
    }
    fn gemini_sessions(&self) -> Vec<Value> {
        self.sessions_of("gemini")
    }
    fn session(&self, id: &str) -> Value {
        self.gemini_sessions()
            .into_iter()
            .find(|s| s["id"] == id)
            .unwrap()
    }
    fn by_conversation(&self, native: &str) -> Value {
        self.gemini_sessions()
            .into_iter()
            .find(|s| s["conversation_id"] == native)
            .unwrap_or_else(|| panic!("no gemini session for {native}"))
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("accounts/gemini").join(name)
    }
    /// Enroll and give the home synthetic credential files (what a finished
    /// login leaves), then refresh so the account is selectable.
    fn add_logged_in(&self, name: &str) {
        let out = self.run(&["account", "add", "--platform", "gemini", name]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        login(&self.home(name));
        let r = self.api(&["account", "refresh", &format!("gemini:{name}")]);
        assert_eq!(r["auth_required"], false, "{r}");
    }
    /// A stopped session discovered from a chat file in the account home.
    fn stopped_session(&self, account: &str) -> (String, String) {
        let native = uuid::Uuid::new_v4().to_string();
        chat(&self.home(account), &self.project, &native);
        let s = self.by_conversation(&native);
        assert_eq!(s["availability"], "stopped");
        (s["id"].as_str().unwrap().to_owned(), native)
    }
    fn send(&self, body: &Value) -> Output {
        let mut child = self
            .command(&["api", "send"])
            .env("GEMINI_API_KEY", "key-must-not-reach-gemini")
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
    fn deletion(&self, id: &str) -> Value {
        let mut last = Value::Null;
        wait_for("deletion worker", || {
            last = self.api(&["account", "deletions", "--request-id", id]);
            matches!(last["phase"].as_str(), Some("complete" | "failed"))
        });
        let lock = self
            .paths
            .root
            .join(sessionsation::layout::ACCOUNT_DELETIONS)
            .join(format!("{id}.worker.lock"));
        wait_for("worker exit", || {
            let f = std::fs::OpenOptions::new().write(true).open(&lock);
            f.is_err() || fs2::FileExt::try_lock_exclusive(&f.unwrap()).is_ok()
        });
        last
    }
    /// An interactive fake Gemini outside Sessionsation (SSH or another terminal).
    fn external(&self, home: &Path, flag: &str, native: &str) -> Process {
        Process(
            Command::new(&self.fake)
                .args([flag, native])
                .env("GEMINI_CLI_HOME", home)
                .current_dir(&self.project)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn kill(mut child: Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    child.wait().unwrap();
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
/// What a completed Login with Google leaves (synthetic content only).
fn login(home: &Path) {
    let dir = home.join(".gemini");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("oauth_creds.json"), "synthetic fixture").unwrap();
    std::fs::write(
        dir.join("google_accounts.json"),
        r#"{"active":"fixture@example.invalid","old":[]}"#,
    )
    .unwrap();
}
fn slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}
/// A chat file with a header (and a private message the API must never echo)
/// plus the project registry entry for its working directory.
fn chat(home: &Path, cwd: &Path, id: &str) -> PathBuf {
    let dir = home.join(".gemini/tmp").join(slug(cwd)).join("chats");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("session-2026-01-01T00-00-{}.jsonl", &id[..8]));
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            json!({"sessionId":id,"projectHash":"fixture","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"user","content":"private prompt"})
        ),
    )
    .unwrap();
    std::fs::write(
        home.join(".gemini/projects.json"),
        json!({"projects":{cwd.to_string_lossy():slug(cwd)}}).to_string(),
    )
    .unwrap();
    path
}
fn find_chat(home: &Path, id: &str) -> PathBuf {
    let suffix = format!("-{}.jsonl", &id[..8]);
    let mut found = vec![];
    fn walk(dir: &Path, suffix: &str, found: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, suffix, found);
            } else if p.to_string_lossy().ends_with(suffix) {
                found.push(p);
            }
        }
    }
    walk(&home.join(".gemini/tmp"), &suffix, &mut found);
    found.pop().unwrap_or_else(|| panic!("no chat for {id}"))
}
fn resume_body(id: &str, session: &Value, text: &str) -> Value {
    json!({"api_version":1,"message_id":id,"mode":"resume","session_id":session["id"],
        "expected_generation":session["generation"],"text":text,"expires_at":now()+600})
}
async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let install = |name: &str, body: &str| {
        let path = tmp.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let gemini = install("gemini.js", include_str!("support/fake-gemini.sh"));
    let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
    let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
        gemini_binary: gemini.to_str().unwrap().into(),
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
        fake: gemini,
        _daemon: daemon,
    }
}

fn operations(f: &Fixture) -> Vec<Value> {
    f.api(&["operations"])
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["inline"] == true)
        .cloned()
        .collect()
}
fn inline_op(f: &Fixture, kind: &str, name: &str) -> Value {
    operations(f)
        .into_iter()
        .find(|o| o["kind"] == kind && o["name"] == name)
        .unwrap_or_else(|| {
            panic!(
                "no inline {kind} operation named {name}: {:?}",
                operations(f)
            )
        })
}
fn reap(mut child: Child) {
    kill_ref(&mut child);
    let _ = child.wait();
}
fn kill_ref(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_advertise_named_journaled_inline_launches() {
    let f = fixture().await;
    let caps = f.api(&["capabilities"]);
    assert_eq!(caps["contract_version"], "1.33.0");
    assert_eq!(caps["contract_revision"], 33);
    let c = &caps["inline_launch"];
    assert_eq!(c["name"], true);
    assert_eq!(c["journaled"], true);
    assert_eq!(c["operations"], true);
    assert_eq!(c["uncertain_never_replayed"], true);
    for p in ["codex", "claude", "gemini", "antigravity"] {
        assert!(c["providers"].as_array().unwrap().iter().any(|v| v == p));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_run_with_name_is_journaled_visible_in_operations_and_settles_when_it_ends() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let child = f
        .command(&["run", "--account", "personal", "--name", "typed name"])
        .current_dir(&f.project)
        .spawn()
        .unwrap();
    wait_for("fake gemini launch", || {
        home.join("fake-launch-new").exists()
    });
    // The catalog entry carries the typed name, not the directory name.
    wait_for("named catalog session", || {
        f.gemini_sessions()
            .iter()
            .any(|s| s["name"] == "typed name")
    });
    assert_eq!(f.gemini_sessions().len(), 1);
    let op = inline_op(&f, "launch", "typed name");
    assert!(
        matches!(op["phase"].as_str(), Some("attached" | "running")),
        "{op}"
    );
    assert_eq!(op["account_id"], "gemini:personal");
    assert_eq!(op["can_recover"], false);
    // The journal is the ordinary launch receipt, marked inline.
    let id = op["id"].as_str().unwrap();
    let receipt = f.api(&["launch", "status", "--request-id", id]);
    assert_eq!(receipt["inline"]["pid"], child.id());
    assert_eq!(receipt["request"]["name"], "typed name");
    assert_eq!(receipt["session_id"], id);
    assert!(
        f.api(&["launch", "list"])
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["request"]["request_id"] == id)
    );
    // A second typed command cannot take the same name.
    let again = f.run(&["run", "--account", "personal", "--name", "typed name"]);
    assert!(String::from_utf8_lossy(&again.stderr).contains("SESSION_NAME_EXISTS"));
    // Ending the inline process settles the journal; nothing is replayed.
    reap(child);
    wait_for("settled journal", || {
        inline_op(&f, "launch", "typed name")["phase"] == "running"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unnamed_inline_run_is_journaled_with_the_directory_name() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let child = f
        .command(&["run", "--account", "personal"])
        .current_dir(&f.project)
        .spawn()
        .unwrap();
    wait_for("fake gemini launch", || {
        home.join("fake-launch-new").exists()
    });
    let op = inline_op(&f, "launch", "project");
    assert_eq!(f.gemini_sessions()[0]["name"], "project");
    assert_eq!(op["session_id"], f.gemini_sessions()[0]["id"]);
    reap(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_names_and_unsupported_combinations_are_refused_before_any_journal() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    for bad in ["a/b", " padded", ".", ""] {
        let out = f
            .command(&["run", "--account", "personal", "--name", bad])
            .current_dir(&f.project)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{bad:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("INVALID_SESSION_NAME"),
            "{bad:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let thread = f.run(&[
        "run",
        "--account",
        "personal",
        "--thread",
        &uuid::Uuid::new_v4().to_string(),
        "--name",
        "x",
    ]);
    assert!(String::from_utf8_lossy(&thread.stderr).contains("INVALID_ARGUMENT"));
    assert!(operations(&f).is_empty());
    assert!(!home.join("fake-launch-new").exists());
    assert!(f.gemini_sessions().is_empty());
    let (id, _) = f.stopped_session("personal");
    let g = f.session(&id)["generation"].as_str().unwrap().to_owned();
    let out = f.run(&[
        "resume",
        &id,
        "--expected-generation",
        &g,
        "--name",
        "bad/name",
    ]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("INVALID_SESSION_NAME"));
    assert!(operations(&f).is_empty());
    assert_eq!(f.session(&id)["generation"], g.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_refused_before_the_provider_is_failed_and_frees_the_name() {
    let f = fixture().await;
    let out = f.run(&["account", "add", "--platform", "gemini", "fresh"]);
    assert!(out.status.success());
    let refused = f
        .command(&["run", "--account", "fresh", "--name", "later"])
        .current_dir(&f.project)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&refused.stderr).contains("AUTH_REQUIRED"));
    let op = inline_op(&f, "launch", "later");
    assert_eq!(op["phase"], "failed");
    assert!(op["detail"].as_str().unwrap().contains("AUTH_REQUIRED"));
    login(&f.home("fresh"));
    f.api(&["account", "refresh", "gemini:fresh"]);
    let child = f
        .command(&["run", "--account", "fresh", "--name", "later"])
        .current_dir(&f.project)
        .spawn()
        .unwrap();
    wait_for("named session after the failed attempt", || {
        f.gemini_sessions().iter().any(|s| s["name"] == "later")
    });
    reap(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_resume_renames_journals_and_settles() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let (id, native) = f.stopped_session("personal");
    let generation = f.session(&id)["generation"].as_str().unwrap().to_owned();
    let child = f
        .command(&[
            "resume",
            &id,
            "--expected-generation",
            &generation,
            "--name",
            "renamed by resume",
        ])
        .current_dir("/")
        .spawn()
        .unwrap();
    wait_for("fake gemini resume", || {
        home.join("fake-launch-resume").exists()
    });
    assert!(read(&home.join("fake-launch-resume")).contains(&format!("session={native}")));
    assert_eq!(f.session(&id)["name"], "renamed by resume");
    let op = inline_op(&f, "resume", "renamed by resume");
    assert_eq!(op["session_id"], id.as_str());
    assert!(
        matches!(op["phase"].as_str(), Some("attached" | "running")),
        "{op}"
    );
    let receipt = f.api(&[
        "resume-launch",
        "status",
        "--request-id",
        op["id"].as_str().unwrap(),
    ]);
    assert_eq!(receipt["inline"]["pid"], child.id());
    assert!(!receipt["previous_name"].as_str().unwrap().is_empty());
    // While the inline provider runs, a second resume is refused (the session is live).
    let g2 = f.session(&id)["generation"].as_str().unwrap().to_owned();
    let second = f.run(&["resume", &id, "--expected-generation", &g2]);
    assert!(!second.status.success());
    reap(child);
    wait_for("settled resume journal", || {
        inline_op(&f, "resume", "renamed by resume")["phase"] == "running"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfinished_resumption_blocks_inline_resume_and_is_never_replayed() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let (id, native) = f.stopped_session("personal");
    let generation = f.session(&id)["generation"].as_str().unwrap().to_owned();
    let prior = uuid::Uuid::new_v4().to_string();
    let dir = f.paths.root.join(sessionsation::layout::RESUMPTIONS);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{prior}.json"));
    std::fs::write(
        &file,
        serde_json::to_vec(&json!({
            "version":1,
            "request":{"version":1,"request_id":prior,"session_id":id,"expected_generation":generation,
                "account_id":"gemini:personal","name":"held","host":{"session":"s","socket":"/s"},
                "space":{"kind":"existing","id":"w1"}},
            "directory":"/","previous_name":"p","conversation_id":native,"generation":generation,
            "phase":"uncertain","detail":null,"workspace_id":null,"tab_id":null,"pane_id":null,
            "operation_id":null,"updated_at":1}))
        .unwrap(),
    )
    .unwrap();
    let before = std::fs::read(&file).unwrap();
    let out = f.run(&[
        "resume",
        &id,
        "--expected-generation",
        &generation,
        "--name",
        "n",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("RESUME_IN_PROGRESS") || stderr.contains("SESSION_OPERATION_IN_PROGRESS"),
        "{stderr}"
    );
    assert!(!home.join("fake-launch-resume").exists());
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(f.session(&id)["generation"], generation.as_str());
    assert!(
        operations(&f).is_empty(),
        "no inline entry for a refused admission"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_herdr_pane_tab_is_labelled_with_the_name() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let bin = f._tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let calls = f._tmp.path().join("herdr-calls");
    let script = bin.join("herdr");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {calls}\nwhile [ \"$1\" = --session ]; do shift 2; done\ncase \"$1 $2\" in\n  \"pane get\") echo '{{\"result\":{{\"pane\":{{\"pane_id\":\"w1:p2\",\"tab_id\":\"w1:t2\"}}}}}}' ;;\n  \"tab rename\") echo '{{\"result\":{{}}}}' ;;\n  \"api snapshot\") echo '{{\"result\":{{\"snapshot\":{{\"panes\":[],\"tabs\":[]}}}}}}' ;;\n  *) echo '{{\"error\":{{\"code\":\"unsupported_fake_command\"}}}}'; exit 1 ;;\nesac\n",
            calls = calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let child = f
        .command(&["run", "--account", "personal", "--name", "tabbed"])
        .current_dir(&f.project)
        .env("PATH", &path)
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w1:p2")
        .env("SESSIONSATION_HERDR_SESSION", "s1")
        .spawn()
        .unwrap();
    wait_for("tab label", || {
        std::fs::read_to_string(&calls)
            .unwrap_or_default()
            .contains("tab rename w1:t2 tabbed")
    });
    assert_eq!(inline_op(&f, "launch", "tabbed")["pane_id"], "w1:p2");
    reap(child);
}
