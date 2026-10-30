//! Gemini CLI provider integration with a metadata-only fake
//! (`support/fake-gemini.sh`, installed as `gemini.js`). Disposable homes only; no
//! provider network, credentials or model requests. Login and headless behaviour
//! are verified against the fake only; see docs/architecture.md.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_returns_the_login_plan_refresh_observes_login_and_unauthenticated_use_is_refused() {
    let f = fixture().await;
    let added = f.api(&["account", "add", "gemini:fresh"]);
    assert_eq!(added["provider"], "gemini");
    assert_eq!(added["existing_home"], false);
    assert_eq!(added["auth"]["problem"], "AUTH_REQUIRED");
    assert_eq!(added["login_required"], true);
    let home = f.home("fresh");
    assert_eq!(
        std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // Settings are seeded: no auto-update, statistics or session deletion.
    let settings: Value = serde_json::from_str(&read(&home.join(".gemini/settings.json"))).unwrap();
    assert_eq!(settings["general"]["enableAutoUpdate"], false);
    assert_eq!(settings["privacy"]["usageStatisticsEnabled"], false);
    assert_eq!(
        settings["security"]["auth"]["selectedType"],
        "oauth-personal"
    );
    let plan = f.api(&["account", "login", "gemini:fresh"]);
    assert_eq!(
        added["login"], plan["login"],
        "add returns the login command's plan"
    );
    assert_eq!(plan["login"]["credentials_in_client"], false);
    let refreshed = f.api(&["account", "refresh", "gemini:fresh"]);
    assert_eq!(refreshed["auth_required"], true);
    assert_eq!(refreshed["auth_detail"], "AUTH_REQUIRED");

    // Unauthenticated: launch and headless delivery are refused before any record.
    let out = f
        .command(&["run", "--account", "gemini:fresh"])
        .current_dir(&f.project)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("AUTH_REQUIRED"));
    assert!(!home.join("fake-launch-new").exists());
    assert!(f.gemini_sessions().is_empty());
    let new = json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":"new",
        "account_id":"gemini:fresh","directory":f.project,"text":"hi","expires_at":now()+600});
    assert!(f.send_err(&new).contains("AUTH_REQUIRED"));
    assert!(f.gemini_sessions().is_empty());

    // Execute the plan: the official interactive command with NO_BROWSER, in the
    // account environment, ambient backend overrides stripped.
    let argv: Vec<String> = plan["login"]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env("GEMINI_API_KEY", "key-must-not-reach-gemini")
        .env("GOOGLE_GENAI_USE_VERTEXAI", "true")
        .env("GOOGLE_CLOUD_PROJECT", "ambient-project")
        .env("GEMINI_MODEL", "ambient-model")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = read(&home.join("fake-login-env"));
    assert!(
        env.contains("api_key=\n")
            && env.contains("vertex=\n")
            && env.contains("project=\n")
            && env.contains("model=\n"),
        "{env}"
    );
    assert!(env.contains(&format!("home={}", home.display())), "{env}");
    assert!(
        env.contains("no_relaunch=1\n") && env.contains("file_storage=true\n"),
        "{env}"
    );
    assert!(env.contains("no_browser=true\n"), "{env}");
    assert!(
        env.contains("args=\n"),
        "interactive gemini takes no arguments: {env}"
    );
    let refreshed = f.api(&["account", "refresh", "gemini:fresh"]);
    assert_eq!(refreshed["auth_required"], false, "{refreshed}");
    assert_eq!(refreshed["auth_mode"]["auth_method"], "oauth-personal");
    assert!(!refreshed.to_string().contains("example.invalid"));
    let row = f.api(&["account", "list", "--platform", "gemini"])[0].clone();
    assert_eq!(row["id"], "gemini:fresh");
    assert_eq!(row["auth_required"], false);
    assert_eq!(row["selectable"], true);

    // Credentials without an active account are not a login.
    std::fs::write(
        home.join(".gemini/google_accounts.json"),
        r#"{"active":null,"old":[]}"#,
    )
    .unwrap();
    assert_eq!(
        f.api(&["account", "refresh", "gemini:fresh"])["auth_required"],
        true
    );

    // An operator-prepared 0700 home is registered; settings keys are not clobbered.
    let prepared = f.home("prepared");
    std::fs::create_dir_all(prepared.join(".gemini")).unwrap();
    std::fs::set_permissions(&prepared, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        prepared.join(".gemini/settings.json"),
        r#"{"theme":"dark","security":{"auth":{"selectedType":"gemini-api-key"}}}"#,
    )
    .unwrap();
    let ready = f.api(&["account", "add", "gemini:prepared"]);
    assert_eq!(ready["existing_home"], true);
    let kept: Value = serde_json::from_str(&read(&prepared.join(".gemini/settings.json"))).unwrap();
    assert_eq!(kept["theme"], "dark");
    assert_eq!(kept["security"]["auth"]["selectedType"], "gemini-api-key");
    assert_eq!(kept["general"]["enableAutoUpdate"], false);
    // A home that is not private is refused and left alone.
    let loose = f.home("loose");
    std::fs::create_dir_all(&loose).unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        f.api_error(&["account", "add", "gemini:loose"])
            .contains("UNSAFE_ACCOUNT_HOME")
    );
    // The human command without a terminal prints and does not start a login.
    let out = f.run(&["account", "add", "--platform", "gemini", "quiet"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("account login gemini:quiet"));
    assert!(!f.home("quiet").join("fake-login-env").exists());
    // Names are unique per provider only.
    let other = f.run(&["account", "add", "--platform", "claude", "fresh"]);
    assert!(other.status.success());
    let clash = f.run(&["account", "add", "--platform", "gemini", "fresh"]);
    assert!(!clash.status.success());
    assert!(String::from_utf8_lossy(&clash.stderr).contains("gemini:fresh"));
    // Codex refresh stays unsupported.
    assert!(
        f.run(&["account", "add", "--platform", "codex", "cx"])
            .status
            .success()
    );
    assert!(
        f.api_error(&["account", "refresh", "codex:cx"])
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_launch_track_live_identity_rename_resume_and_go_stale() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let launcher = f.api(&["launcher", "new", "--account", "gemini:personal"]);
    let argv: Vec<_> = launcher["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert!(
        argv.ends_with(&["run".into(), "--account".into(), "gemini:personal".into()]),
        "{argv:?}"
    );

    // New launch: ambient backend overrides stripped, id chosen and recorded first.
    let child = f
        .command(&["run", "--account", "personal"])
        .current_dir(&f.project)
        .env("GEMINI_API_KEY", "key-must-not-reach-gemini")
        .env("GOOGLE_GEMINI_BASE_URL", "https://example.invalid")
        .spawn()
        .unwrap();
    wait_for("fake gemini launch", || {
        home.join("fake-launch-new").exists()
    });
    let launch = read(&home.join("fake-launch-new"));
    assert!(
        launch.contains("api_key=\n") && launch.contains("base_url=\n"),
        "{launch}"
    );
    assert!(
        launch.contains("no_relaunch=1\n") && launch.contains("file_storage=true\n"),
        "{launch}"
    );
    assert!(
        launch.contains(&format!("home={}", home.display())),
        "{launch}"
    );
    assert!(
        launch.contains(&format!("cwd={}", f.project.display())),
        "{launch}"
    );
    let native = launch
        .lines()
        .find_map(|l| l.strip_prefix("session="))
        .unwrap()
        .to_owned();
    uuid::Uuid::parse_str(&native).unwrap();
    assert!(
        launch.contains(&format!(
            "args=--session-id {native} --approval-mode yolo\n"
        )),
        "{launch}"
    );
    wait_for("live gemini session", || {
        f.gemini_sessions()
            .first()
            .is_some_and(|s| s["availability"] == "running")
    });
    let sessions = f.gemini_sessions();
    assert_eq!(
        sessions.len(),
        1,
        "launch and chat discovery are one session"
    );
    let s = &sessions[0];
    assert_eq!(s["conversation_id"], native);
    assert_eq!(s["conversation_id_source"], "native");
    assert_eq!(s["account_id"], "gemini:personal");
    assert_eq!(s["pid"], child.id());
    assert_eq!(s["directory"], json!(f.project));
    let started = s["active_since"].as_i64().expect("gemini process start");
    assert!((now() - 60..=now() + 1).contains(&started), "{started}");
    assert_eq!(s["capabilities"]["resume"], false);
    assert_eq!(s["capabilities"]["submit"], false);
    assert_eq!(s["capabilities"]["add_to_herdr"], false);
    assert!(
        s["capability_reasons"]["add_to_herdr"]
            .as_str()
            .unwrap()
            .starts_with("NO_NATIVE_ATTACH")
    );
    assert!(
        !f.api(&["state"]).to_string().contains("private prompt"),
        "chat text must never surface"
    );
    let id = s["id"].as_str().unwrap().to_owned();
    let generation = s["generation"].as_str().unwrap().to_owned();
    let details = f.api(&[
        "session",
        "--session",
        &id,
        "--expected-generation",
        &generation,
    ]);
    assert_eq!(details["provider"], "gemini");
    assert_eq!(details["availability"], "running");
    // The identity is stable across repeated reads: no duplicates.
    for _ in 0..3 {
        assert_eq!(f.gemini_sessions().len(), 1);
    }

    // Live sessions are not resumed again; lifecycle control is not claimed.
    let out = f.run(&["resume", &id, "--expected-generation", &generation]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("SESSION_ALREADY_RUNNING"));
    assert!(
        f.api_error(&[
            "stop",
            "--session",
            &id,
            "--expected-generation",
            &generation
        ])
        .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
    kill(child);

    let s = f.session(&id);
    assert_eq!(s["availability"], "stopped");
    assert!(s["active_since"].is_null(), "{s}");
    assert_eq!(s["capabilities"]["resume"], true);
    assert!(s["pid"].is_null());
    let away = f.project.with_extension("away");
    std::fs::rename(&f.project, &away).unwrap();
    let gone = f.session(&id);
    std::fs::rename(&away, &f.project).unwrap();
    assert_eq!(gone["capabilities"]["resume"], false);
    assert_eq!(
        gone["capability_reasons"]["resume"],
        "SAVED_DIRECTORY_UNAVAILABLE"
    );

    // Label rename: identity unchanged, no native rename.
    let generation = s["generation"].as_str().unwrap().to_owned();
    let renamed = f.api(&[
        "rename",
        "--session",
        &id,
        "--expected-generation",
        &generation,
        "--name",
        "relabeled",
    ]);
    assert_eq!(renamed["name"], "relabeled");
    assert_eq!(renamed["name_source"], "catalog");
    assert_eq!(renamed["native_rename"]["supported"], false);
    assert!(
        f.api_error(&[
            "rename",
            "--session",
            &id,
            "--expected-generation",
            &generation,
            "--name",
            "again"
        ])
        .contains("STALE_TARGET")
    );
    let s = f.session(&id);
    assert_eq!(s["name"], "relabeled");
    assert_eq!(s["conversation_id"], native);

    // Resume runs `-r <uuid>` in the recorded directory.
    let generation = s["generation"].as_str().unwrap().to_owned();
    let plan = f.api(&[
        "launcher",
        "resume",
        "--session",
        &id,
        "--expected-generation",
        &generation,
    ]);
    assert_eq!(plan["cwd"], json!(f.project));
    let child = f
        .command(&["resume", &id, "--expected-generation", &generation])
        .current_dir("/")
        .spawn()
        .unwrap();
    wait_for("fake gemini resume", || {
        home.join("fake-launch-resume").exists()
    });
    let resume = read(&home.join("fake-launch-resume"));
    assert!(resume.contains(&format!("session={native}")), "{resume}");
    assert!(resume.contains(&format!("args=-r {native}")), "{resume}");
    assert!(
        resume.contains(&format!("cwd={}", f.project.display())),
        "{resume}"
    );
    wait_for("resumed session live", || {
        f.session(&id)["availability"] == "running"
    });
    assert_eq!(f.session(&id)["pid"], child.id());
    assert_eq!(
        f.gemini_sessions().len(),
        1,
        "resume re-attaches, not duplicates"
    );
    kill(child);

    // Missing chat file: stale, and resume refuses.
    std::fs::remove_file(find_chat(&home, &native)).unwrap();
    let s = f.session(&id);
    assert_eq!(s["availability"], "stale");
    let out = f.run(&[
        "resume",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
    ]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("SESSION_STALE"));
    assert_eq!(f.session(&id)["name"], "relabeled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_processes_map_to_one_entry_and_accounts_and_providers_are_isolated() {
    let f = fixture().await;
    f.add_logged_in("one");
    f.add_logged_in("two");
    assert!(
        f.run(&["account", "add", "--platform", "claude", "cl"])
            .status
            .success()
    );
    let (one, two) = (f.home("one"), f.home("two"));

    // A session started in another terminal (not by Sessionsation).
    let native = uuid::Uuid::new_v4().to_string();
    let live = f.external(&one, "--session-id", &native);
    wait_for("external session discovered", || {
        f.gemini_sessions()
            .iter()
            .any(|s| s["conversation_id"] == native.as_str() && s["availability"] == "running")
    });
    let s = f.by_conversation(&native);
    assert_eq!(s["account_id"], "gemini:one");
    assert_eq!(s["pid"], live.0.id());
    assert!(s["attachment"].is_null(), "not in a Sessionsation terminal");
    for _ in 0..3 {
        assert_eq!(
            f.gemini_sessions()
                .iter()
                .filter(|s| s["conversation_id"] == native.as_str())
                .count(),
            1
        );
    }
    // The same native id under another provider is another conversation; a chat of
    // another account is not held by the process of this account's home.
    let other = uuid::Uuid::new_v4().to_string();
    chat(&two, &f.project, &other);
    let claude_dir = f
        .paths
        .root
        .join("accounts/claude/cl/projects")
        .join(f.project.to_string_lossy().replace('/', "-"));
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join(format!("{native}.jsonl")),
        format!(
            "{}\n",
            json!({"type":"user","sessionId":native,"cwd":f.project,"timestamp":"2026-01-01T00:00:00Z"})
        ),
    )
    .unwrap();
    let all = f.api(&["state"])["sessions"].as_array().unwrap().clone();
    let same: Vec<_> = all
        .iter()
        .filter(|s| s["conversation_id"] == native.as_str())
        .collect();
    assert_eq!(same.len(), 2, "{same:?}");
    let in_two = f.by_conversation(&other);
    assert_eq!(
        in_two["availability"], "stopped",
        "a process of another home does not hold this account's session"
    );
    let claude = same.iter().find(|s| s["provider"] == "claude").unwrap();
    assert_eq!(claude["availability"], "stopped");
    assert_eq!(
        same.iter()
            .find(|s| s["account_id"] == "gemini:one")
            .unwrap()["availability"],
        "running"
    );
    // Membership removal (a verified-process check, not the fast intent preview)
    // is refused while a verified process uses the home, and allowed for the other.
    let preview = f.api(&["account", "remove", "gemini:one"]);
    assert_eq!(preview["can_remove"], false, "{preview}");
    assert_eq!(
        f.api(&["account", "remove", "gemini:two"])["can_remove"],
        true
    );
    drop(live);
    assert_eq!(f.by_conversation(&native)["availability"], "stopped");
    // A Codex session is untouched by Gemini providers in the catalog.
    let codex =
        sessionsation::catalog::record_launch(&f.paths, "cx", "thread", "codex", None).unwrap();
    assert!(
        f.gemini_sessions()
            .iter()
            .all(|s| s["id"] != codex.id.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_resume_and_new_deliver_once_with_durable_receipts() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let (id, native) = f.stopped_session("personal");
    let session = f.session(&id);

    // Success: stopped session, account environment, recorded directory, stdin message.
    let m1 = uuid::Uuid::new_v4().to_string();
    let body = resume_body(&m1, &session, "hello world");
    let first = f.send_ok(&body);
    assert_eq!(first["provider"], "gemini");
    assert_eq!(first["delivery_semantics"], "headless_print");
    let r = f.settled(&m1);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["reply"]["text"], "fake reply to: hello world");
    assert_eq!(r["reply"]["session_id"], native.as_str());
    assert_eq!(r["correlation"]["matched"], true);
    assert_eq!(r["facts"]["reply_observed"], true);
    assert_eq!(r["session_id"], id.as_str());
    assert_eq!(r["provider"], "gemini");
    assert_eq!(r["account_id"], "gemini:personal");
    assert_eq!(r["provider_conversation_id"], native.as_str());
    let launch = read(&home.join("fake-print-resume"));
    assert!(launch.contains(&format!("session={native}")), "{launch}");
    assert!(launch.contains("message=hello world"), "{launch}");
    assert!(
        launch.contains(&format!("cwd={}", f.project.display())),
        "{launch}"
    );
    assert!(
        launch.contains("api_key=\n") && launch.contains("no_relaunch=1\n"),
        "{launch}"
    );
    assert!(launch.contains("trust=true\n"), "{launch}");
    assert!(launch.contains("approval=default\n"), "{launch}");
    assert!(!launch.contains("yolo"), "never an auto-approve mode");
    assert!(
        launch.contains(&format!(
            "args=-p  -o json --approval-mode default -r {native}"
        )),
        "{launch}"
    );
    assert!(
        launch.contains(&format!("home={}", home.display())),
        "{launch}"
    );
    assert_eq!(read(&home.join("fake-print-count")).trim(), "1");

    // Idempotent retry returns the same receipt without executing again.
    let again = f.send_ok(&body);
    assert_eq!(again["state"], "completed_with_reply");
    assert_eq!(again["reply"], r["reply"]);
    assert_eq!(read(&home.join("fake-print-count")).trim(), "1");
    let mut changed = body.clone();
    changed["text"] = json!("different");
    assert!(f.send_err(&changed).contains("MESSAGE_ID_CONFLICT"));

    // Auth gate before admission: nothing journaled or executed.
    let accounts = home.join(".gemini/google_accounts.json");
    let saved = read(&accounts);
    std::fs::remove_file(&accounts).unwrap();
    let session = f.session(&id);
    let refused = uuid::Uuid::new_v4().to_string();
    let err = f.send_err(&resume_body(&refused, &session, "x"));
    assert!(err.contains("AUTH_REQUIRED"), "{err}");
    std::fs::write(&accounts, saved).unwrap();
    assert!(
        f.api_error(&["receipt", &refused])
            .contains("receipt_not_found")
    );

    // A provider-reported error is a failed (not delivered) outcome.
    std::fs::write(home.join("fake-print-error"), "1").unwrap();
    let bad = uuid::Uuid::new_v4().to_string();
    f.send_ok(&resume_body(&bad, &f.session(&id), "will fail"));
    let r = f.settled(&bad);
    std::fs::remove_file(home.join("fake-print-error")).unwrap();
    assert_eq!(r["state"], "failed", "{r}");
    assert_eq!(r["reason"], "PROVIDER_REPORTED_ERROR");

    // Session-id mismatch is never success.
    std::fs::write(
        home.join("fake-print-session"),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    let mismatch = uuid::Uuid::new_v4().to_string();
    f.send_ok(&resume_body(&mismatch, &f.session(&id), "mismatch"));
    let r = f.settled(&mismatch);
    std::fs::remove_file(home.join("fake-print-session")).unwrap();
    assert_eq!(r["state"], "delivery_unknown", "{r}");
    assert_eq!(r["reason"], "SESSION_ID_MISMATCH");
    assert_eq!(r["correlation"]["matched"], false);
    assert!(r["reply"].is_null());
    assert_eq!(r["facts"]["reply_observed"], false);

    // Timeout: uncertain, recorded once, never replayed.
    std::fs::write(home.join("fake-print-sleep"), "30").unwrap();
    let slow = uuid::Uuid::new_v4().to_string();
    let mut body = resume_body(&slow, &f.session(&id), "slow");
    body["timeout_seconds"] = json!(1);
    f.send_ok(&body);
    let r = f.settled(&slow);
    std::fs::remove_file(home.join("fake-print-sleep")).unwrap();
    assert_eq!(r["state"], "delivery_unknown", "{r}");
    assert_eq!(r["reason"], "EXECUTION_TIMEOUT");
    assert_eq!(r["execution"]["automatic_replay"], false);
    let count = read(&home.join("fake-print-count"));
    let retry = f.send_ok(&body);
    assert_eq!(retry["state"], "delivery_unknown");
    assert_eq!(read(&home.join("fake-print-count")), count, "no replay");

    // Stale and live sessions are refused before admission.
    let (stale, stale_native) = f.stopped_session("personal");
    std::fs::remove_file(find_chat(&home, &stale_native)).unwrap();
    let s = f.session(&stale);
    assert_eq!(s["availability"], "stale");
    let err = f.send_err(&resume_body(&uuid::Uuid::new_v4().to_string(), &s, "x"));
    assert!(err.contains("SESSION_STALE"), "{err}");
    let live_native = uuid::Uuid::new_v4().to_string();
    let live = f.external(&home, "--session-id", &live_native);
    let mut live_session = Value::Null;
    wait_for("live session", || {
        live_session = f
            .gemini_sessions()
            .into_iter()
            .find(|s| s["conversation_id"] == live_native.as_str())
            .unwrap_or(Value::Null);
        live_session["availability"] == "running"
    });
    let err = f.send_err(&resume_body(
        &uuid::Uuid::new_v4().to_string(),
        &live_session,
        "x",
    ));
    drop(live);
    assert!(err.contains("SESSION_ALREADY_RUNNING"), "{err}");

    // Live input modes need a running session in a HERDR pane (terminal input).
    let mut submit = resume_body(&uuid::Uuid::new_v4().to_string(), &f.session(&id), "x");
    submit["mode"] = json!("submit");
    let err = f.send_err(&submit);
    assert!(err.contains("SESSION_NOT_RUNNING"), "{err}");

    // New conversation: catalog entry first, pre-assigned --session-id.
    let created = uuid::Uuid::new_v4().to_string();
    let new = json!({"api_version":1,"message_id":created,"mode":"new","account_id":"gemini:personal",
        "directory":f.project,"name":"dm-new","text":"start here","expires_at":now()+600});
    let admitted = f.send_ok(&new);
    let catalog_id = admitted["session_id"].as_str().unwrap().to_owned();
    let conversation = admitted["provider_conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = f.settled(&created);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["execution"]["new_conversation"], true);
    assert_eq!(r["reply"]["session_id"], conversation.as_str());
    let launched = read(&home.join("fake-print-new"));
    assert!(
        launched.contains(&format!("session={conversation}")),
        "{launched}"
    );
    assert!(
        launched.contains(&format!("--session-id {conversation}")),
        "{launched}"
    );
    assert!(launched.contains("approval=default\n"), "{launched}");
    let s = f.session(&catalog_id);
    assert_eq!(s["name"], "dm-new");
    assert_eq!(s["conversation_id"], conversation.as_str());
    assert_eq!(s["availability"], "stopped");
    assert_eq!(
        f.gemini_sessions()
            .iter()
            .filter(|s| s["conversation_id"] == conversation.as_str())
            .count(),
        1
    );
    let mut bad = new.clone();
    bad["message_id"] = json!(uuid::Uuid::new_v4().to_string());
    bad["directory"] = json!("relative");
    assert!(f.send_err(&bad).contains("SAVED_DIRECTORY_UNAVAILABLE"));
    assert!(f.api(&["state"]).to_string().find("start here").is_none());
    // A Claude account id is not valid for a Gemini-only mismatch of provider.
    let mut wrong = new.clone();
    wrong["message_id"] = json!(uuid::Uuid::new_v4().to_string());
    wrong["account_id"] = json!("codex:personal");
    assert!(
        f.send_err(&wrong)
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );

    // Capabilities advertise headless delivery with the approval mode.
    let caps = f.api(&["capabilities"]);
    let gemini = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "gemini")
        .unwrap()
        .clone();
    assert_eq!(gemini["operations"]["headless_delivery"]["supported"], true);
    assert_eq!(gemini["headless_delivery"]["automatic_replay"], false);
    assert_eq!(gemini["headless_delivery"]["approval_mode"], "default");
    assert_eq!(caps["contract_revision"], 30);
    assert_eq!(
        caps["providers"][0]["operations"]["headless_delivery"]["supported"],
        false
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn membership_removal_restore_rename_and_local_deletion_match_the_other_providers() {
    let f = fixture().await;
    f.add_logged_in("gone");
    let home = f.home("gone");
    let (id, native) = f.stopped_session("gone");
    std::fs::create_dir_all(home.join(".gemini/history")).unwrap();
    std::fs::write(home.join(".gemini/history/marker"), b"history").unwrap();

    // Display-name rename: identity, name and home unchanged.
    let renamed = f.api(&[
        "account",
        "rename",
        "gemini:gone",
        "--display-name",
        "Pro-Mail",
    ]);
    assert_eq!(renamed["display_name"], "Pro-Mail");
    assert_eq!(renamed["account_id"], "gemini:gone");
    assert!(home.is_dir());
    assert!(
        f.api_error(&[
            "account",
            "rename",
            "gemini:gone",
            "--display-name",
            "gone2x y"
        ])
        .contains("INVALID_DISPLAY_NAME")
    );
    let row = f.api(&["account", "list"])[0].clone();
    assert_eq!(row["display_name"], "Pro-Mail");
    assert_eq!(row["name"], "gone");
    f.api(&["account", "rename", "gemini:gone", "--clear"]);

    // Membership removal is reversible and leaves the data untouched.
    let preview = f.api(&["account", "remove", "gemini:gone"]);
    assert_eq!(preview["can_remove"], true, "{preview}");
    let revision = preview["revision"].as_str().unwrap().to_owned();
    let removed = f.api(&[
        "account",
        "remove",
        "gemini:gone",
        "--confirm",
        "gemini:gone",
        "--revision",
        &revision,
    ]);
    assert_eq!(removed["removed"], true);
    assert_eq!(f.api(&["account", "list"])[0]["selectable"], false);
    assert!(
        f.api_error(&["launcher", "new", "--account", "gemini:gone"])
            .contains("ACCOUNT_REMOVED")
    );
    let s = f.session(&id);
    let err = f.send_err(&resume_body(&uuid::Uuid::new_v4().to_string(), &s, "x"));
    assert!(err.contains("ACCOUNT_REMOVED"), "{err}");
    assert!(home.join(".gemini/oauth_creds.json").exists());
    assert_eq!(
        f.api(&["account", "restore", "gemini:gone"])["restored"],
        true
    );
    assert_eq!(f.api(&["account", "list"])[0]["selectable"], true);
    assert!(
        f.api_error(&["account", "restore", "gemini:gone"])
            .contains("ACCOUNT_NOT_REMOVED")
    );

    // Deletion refuses while a verified process uses the home.
    let live = f.external(&home, "--session-id", &uuid::Uuid::new_v4().to_string());
    wait_for("live process", || {
        f.gemini_sessions()
            .iter()
            .any(|s| s["availability"] == "running")
    });
    let preview = f.api(&["account", "delete", "gemini:gone"]);
    assert_eq!(preview["home_disposition"], "deleted");
    assert_eq!(preview["deletes_local_credentials"], true);
    let request = uuid::Uuid::new_v4().to_string();
    f.api(&[
        "account",
        "delete",
        "gemini:gone",
        "--confirm",
        "gemini:gone",
        "--request-id",
        &request,
    ]);
    let failed = f.deletion(&request);
    drop(live);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(failed["phase"], "failed", "{failed}");
    assert!(
        failed["detail"]
            .as_str()
            .unwrap()
            .contains("ACCOUNT_HAS_RUNNING"),
        "{failed}"
    );
    assert_eq!(failed["can_recover"], true);
    assert!(home.join(".gemini/oauth_creds.json").exists());

    // Explicit recovery continues the original request: home and credentials
    // are deleted, conversations preserved without credentials.
    f.api(&["recover", &request]);
    let done = f.deletion(&request);
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["history_preserved"], true);
    assert!(!home.exists(), "managed home and credentials removed");
    let vault = f
        .paths
        .root
        .join(sessionsation::layout::PRESERVED_CONVERSATIONS)
        .join(&request)
        .join("home/.gemini");
    assert_eq!(read(&vault.join("history/marker")), "history");
    assert!(vault.join("projects.json").is_file());
    let preserved = find_chat(
        &f.paths
            .root
            .join(sessionsation::layout::PRESERVED_CONVERSATIONS)
            .join(&request)
            .join("home"),
        &native,
    );
    assert!(preserved.is_file());
    assert!(!vault.join("oauth_creds.json").exists());
    assert!(!vault.join("google_accounts.json").exists());
    assert!(!vault.join("settings.json").exists());
    assert!(f.api(&["account", "list"]).as_array().unwrap().is_empty());
    let s = f.session(&id);
    assert_eq!(s["availability"], "stopped");
    assert_eq!(s["resume_block_reason"], "account_deleted");
    assert_eq!(s["capabilities"]["resume"], false);
    assert_eq!(s["capability_reasons"]["resume"], "ACCOUNT_DELETED");
    let out = f.run(&[
        "resume",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
    ]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("ACCOUNT_DELETED"));
    let err = f.send_err(&resume_body(&uuid::Uuid::new_v4().to_string(), &s, "x"));
    assert!(err.contains("ACCOUNT_DELETED"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_is_unsupported_not_zero_and_herdr_placement_resumes_but_cannot_attach() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    // Usage: explicit unsupported state, no windows, never a 0% reading.
    let row = f.api(&["account", "list", "--platform", "gemini"])[0].clone();
    assert_eq!(row["usage_state"], "unsupported", "{row}");
    assert_eq!(row["usage_fresh"], false);
    assert!(row["usage"]["short"].is_null() && row["usage"]["weekly"].is_null());
    assert_eq!(row["usage"]["state"], "unsupported");
    assert!(!row["usage"].to_string().contains("used_percentage"));
    let table = String::from_utf8_lossy(&f.run(&["account", "list"]).stdout).into_owned();
    assert!(table.contains("unsupported"), "{table}");
    assert!(!table.contains("0%"), "{table}");
    let caps = f.api(&["capabilities"]);
    let gemini = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "gemini")
        .unwrap()
        .clone();
    assert_eq!(gemini["usage"], false);
    assert_eq!(gemini["usage_state"], "unsupported");
    assert_eq!(gemini["native_rename"], false);
    assert_eq!(gemini["login"], "interactive_exec");
    assert_eq!(gemini["account_refresh"], true);
    assert_eq!(gemini["operations"]["usage"]["supported"], false);
    assert_eq!(gemini["operations"]["native_rename"]["supported"], false);
    assert_eq!(
        caps["add_to_herdr"]["unsupported"]["gemini"],
        "NO_NATIVE_ATTACH"
    );
    assert_eq!(
        caps["session_identity"]["gemini_process_source"],
        "cmdline_session_id_argument"
    );
    let limits = f.api(&["account", "limits", "gemini:personal"]);
    assert_eq!(limits["usage_state"], "unsupported");
    assert!(
        f.api_error(&[
            "account",
            "limits",
            "gemini:personal",
            "--short-used",
            "50",
            "--weekly-used",
            "50"
        ])
        .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );

    // HERDR: Start Session resumes into an existing Space with a tab name; Add to
    // HERDR is refused with the reason.
    let tmp = f.paths.root.parent().unwrap().to_path_buf();
    let bin = tmp.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let herdr = bin.join("herdr");
    std::fs::write(&herdr, include_str!("support/fake-herdr.sh")).unwrap();
    std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = tmp.join("herdr-state");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let hosted = |args: &[&str]| {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        f.command(&all)
            .env("PATH", &path)
            .env("FAKE_HERDR_DIR", &state)
            .output()
            .unwrap()
    };
    let (id, native) = f.stopped_session("personal");
    let s = f.session(&id);
    assert_eq!(s["capabilities"]["add_to_herdr"], false);
    assert!(
        s["capability_reasons"]["add_to_herdr"]
            .as_str()
            .unwrap()
            .starts_with("SESSION_NOT_RUNNING")
    );
    let out = hosted(&[
        "session",
        "add-to-herdr",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success()
            && err.contains("UNSUPPORTED_PROVIDER_OPERATION")
            && err.contains("NO_NATIVE_ATTACH"),
        "{err}"
    );
    assert!(!read(&state.join("calls")).contains("tab create"));
    let out = hosted(&[
        "resume",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
        "--name",
        "gemini-tab",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resumed = serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone();
    assert_eq!(resumed["state"], "running", "{resumed}");
    assert_eq!(resumed["target"], "herdr/fixture/w1:p2");
    assert_eq!(read(&state.join("label-w1:p2")).trim(), "gemini-tab");
    let launch = read(&home.join("fake-launch-resume"));
    assert!(launch.contains(&format!("args=-r {native}")), "{launch}");
    assert!(
        launch.contains(&format!("cwd={}", f.project.display())),
        "{launch}"
    );
    let live = f.session(&id);
    assert_eq!(live["name"], "gemini-tab");
    assert_eq!(live["availability"], "running");
    assert_eq!(live["attachment"]["host"], "herdr");
    assert_eq!(live["attachment"]["target"], "herdr/fixture/w1:p2");
    assert_eq!(f.gemini_sessions().len(), 1, "one entry, not a duplicate");
    // Running in HERDR already: never a second tab.
    assert!(
        live["capability_reasons"]["add_to_herdr"]
            .as_str()
            .unwrap()
            .contains("ALREADY_IN_HERDR")
    );
    if let Ok(pid) = read(&state.join("pid-w1:p2")).trim().parse::<i32>() {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_logout_clears_the_cached_credentials_like_the_cli_and_refuses_in_use() {
    let f = fixture().await;
    f.add_logged_in("me");
    let home = f.home("me");
    // A live gemini process using the home refuses the logout.
    let script = home.parent().unwrap().join("gemini.js");
    std::fs::write(&script, "sleep 600; :\n").unwrap();
    let agent = Process(
        Command::new("sh")
            .arg(&script)
            .env("GEMINI_CLI_HOME", &home)
            .spawn()
            .unwrap(),
    );
    let pid = agent.0.id();
    wait_for("agent exec", || {
        read(Path::new(&format!("/proc/{pid}/cmdline"))).contains("gemini.js")
    });
    let refused = f.api_error(&["account", "logout", "gemini:me"]);
    assert!(refused.contains("ACCOUNT_IN_USE"), "{refused}");
    assert!(home.join(".gemini/oauth_creds.json").exists());
    drop(agent);

    let out = f.api(&["account", "logout", "gemini:me"]);
    assert_eq!(out["account_id"], "gemini:me");
    assert_eq!(out["provider"], "gemini");
    assert_eq!(out["logged_out"], true);
    assert_eq!(out["auth_required"], true);
    assert!(
        out["method"]
            .as_str()
            .unwrap()
            .contains("clearCachedCredentialFile")
    );
    assert!(!home.join(".gemini/oauth_creds.json").exists());
    let accounts: Value =
        serde_json::from_str(&read(&home.join(".gemini/google_accounts.json"))).unwrap();
    assert_eq!(accounts["active"], Value::Null);
    assert_eq!(accounts["old"], json!(["fixture@example.invalid"]));
    assert!(!out.to_string().contains("example.invalid"));
    let row = f.api(&["account", "list", "--platform", "gemini"])[0].clone();
    assert_eq!(row["auth_required"], true);
    assert_eq!(row["selectable"], false);
    assert_eq!(row["usage"]["observed_at"], Value::Null);
    assert_eq!(
        f.api(&["account", "refresh", "gemini:me"])["auth_required"],
        true,
        "the supervisor's probe agrees"
    );
    assert_eq!(
        f.api(&["account", "login", "gemini:me"])["login"]["action"],
        "exec"
    );
    // Idempotent; and a fresh login works again.
    let again = f.api(&["account", "logout", "gemini:me"]);
    assert_eq!(again["logged_out"], true);
    let accounts: Value =
        serde_json::from_str(&read(&home.join(".gemini/google_accounts.json"))).unwrap();
    assert_eq!(accounts["old"], json!(["fixture@example.invalid"]));
    login(&home);
    assert_eq!(
        f.api(&["account", "refresh", "gemini:me"])["auth_required"],
        false
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_state_serves_the_last_snapshot_without_scanning_or_writing() {
    let f = fixture().await;
    for account in ["a", "b", "c"] {
        f.add_logged_in(account);
        for _ in 0..15 {
            chat(
                &f.home(account),
                &f.project,
                &uuid::Uuid::new_v4().to_string(),
            );
        }
    }
    // No snapshot yet: the first cached call computes (and stores) a full state.
    let first = f.api(&["state", "--cached"]);
    assert_eq!(first["snapshot"]["cached"], false);
    assert_eq!(first["sessions"].as_array().unwrap().len(), 45);
    let started = Instant::now();
    let full = f.api(&["state"]);
    let full_ms = started.elapsed();
    assert_eq!(full["snapshot"]["cached"], false);

    // A transcript that appears afterwards is invisible to the cached read,
    // which neither scans nor registers it, but visible to the next full state.
    let late = uuid::Uuid::new_v4().to_string();
    chat(&f.home("a"), &f.project, &late);
    let started = Instant::now();
    let cached = f.api(&["state", "--cached"]);
    let cached_ms = started.elapsed();
    assert_eq!(cached["snapshot"]["cached"], true);
    assert_eq!(cached["snapshot"]["fresh"], true);
    assert_eq!(cached["snapshot"]["observed_at"], full["observed_at"]);
    assert_eq!(cached["sessions"], full["sessions"]);
    assert!(
        !cached.to_string().contains(&late),
        "cached state must not scan transcripts"
    );
    assert_eq!(cached["capabilities"]["state_cached"], true);
    eprintln!("state timing: full {full_ms:?}, cached {cached_ms:?} (45 transcripts)");
    let refreshed = f.api(&["state"]);
    assert!(refreshed.to_string().contains(&late));
    assert!(f.api(&["state", "--cached"]).to_string().contains(&late));
}

/// Revision 17: Gemini chats archive, restore and delete like Claude transcripts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_chats_archive_restore_and_delete_and_running_ones_are_refused() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let (mine, other) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    let path = chat(&home, &f.project, &mine);
    let kept = chat(&home, &f.project, &other);
    let body = read(&path);
    let job = |command: &str, s: &Value| {
        let request = uuid::Uuid::new_v4().to_string();
        let id = s["id"].as_str().unwrap();
        f.api(&[
            command,
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--request-id",
            &request,
            "--confirm",
            id,
        ]);
        let mut last = Value::Null;
        wait_for(command, || {
            last = f
                .api(&["operations"])
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["id"] == request.as_str())
                .cloned()
                .unwrap_or(Value::Null);
            matches!(last["phase"].as_str(), Some("complete" | "failed"))
        });
        assert_eq!(last["phase"], "complete", "{last}");
    };
    let all = || {
        f.api(&["state", "--archive-filter", "all"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["provider"] == "gemini")
            .cloned()
            .collect::<Vec<_>>()
    };

    // A running chat is refused.
    let live = f.external(&home, "-r", &mine);
    wait_for("running", || {
        f.by_conversation(&mine)["availability"] == "running"
    });
    let s = f.by_conversation(&mine);
    assert_eq!(s["capabilities"]["archive"], false);
    let id = s["id"].as_str().unwrap().to_owned();
    let e = f.api_error(&[
        "archive",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--request-id",
        &uuid::Uuid::new_v4().to_string(),
        "--confirm",
        &id,
    ]);
    assert!(e.contains("SESSION_RUNNING"), "{e}");
    drop(live);
    wait_for("stopped", || {
        f.by_conversation(&mine)["availability"] == "stopped"
    });

    job("archive", &f.by_conversation(&mine));
    assert!(!path.exists() && kept.exists());
    let archived = all().into_iter().find(|s| s["id"] == id.as_str()).unwrap();
    assert_eq!(archived["availability"], "archived");
    job("restore-session", &archived);
    assert_eq!(read(&path), body);
    job("delete-session", &f.by_conversation(&mine));
    assert!(!path.exists() && kept.exists());
    assert!(!all().iter().any(|s| s["id"] == id.as_str()));
}

#[path = "support/fake_launch_host.rs"]
mod fake_launch_host;

/// The UI path (`api launch start` -> HERDR pane -> `launch-run`) with no
/// `launch_permissions` in the data-root config starts Gemini in yolo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_launch_without_configured_permissions_starts_gemini_in_yolo() {
    let f = fixture().await;
    let config = f.paths.root.join("config.json");
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    saved.as_object_mut().unwrap().remove("launch_permissions");
    std::fs::write(&config, saved.to_string()).unwrap();
    f.add_logged_in("personal");
    let home = f.home("personal");
    let dir = f.paths.root.parent().unwrap().to_owned();
    let _pane = fake_launch_host::launch(binary(), &f.paths.root, &dir, "gemini:personal", "ui");
    wait_for("gemini launch", || home.join("fake-launch-new").exists());
    let launch = read(&home.join("fake-launch-new"));
    let native = launch
        .lines()
        .find_map(|l| l.strip_prefix("session="))
        .unwrap()
        .to_owned();
    assert!(
        launch.contains(&format!(
            "args=--session-id {native} --approval-mode yolo\n"
        )),
        "{launch}"
    );
}

/// Run with `input` on stdin and return (status ok, stdout, stderr).
fn with_stdin(f: &Fixture, args: &[&str], input: &str) -> (bool, String, String) {
    use std::io::Write;
    let mut child = f
        .command(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
/// Every file under `dir` (recursively) except `skip`, as lossy text.
fn every_file_text(dir: &Path, skip: &Path, out: &mut Vec<(PathBuf, String)>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            every_file_text(&p, skip, out);
        } else if t.is_file() && p != skip {
            let bytes = std::fs::read(&p).unwrap_or_default();
            out.push((p, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_mode_stores_the_key_privately_injects_it_by_env_validates_and_never_echoes_it() {
    use sha2::{Digest, Sha256};
    let f = fixture().await;
    f.add_logged_in("keyed");
    let home = f.home("keyed");
    let id = "gemini:keyed";
    let caps = f.api(&["capabilities"]);
    let gemini = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "gemini")
        .unwrap()
        .clone();
    let modes: Vec<_> = gemini["auth_modes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["available"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        modes,
        [
            ("google_personal", false),
            ("google_workspace", true),
            ("api_key", true)
        ]
    );
    assert!(
        gemini["auth_modes"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("retired")
    );

    let status = f.api(&["account", "auth", id]);
    assert_eq!(
        (
            status["auth_mode"].as_str(),
            status["api_key_present"].as_bool()
        ),
        (Some("google"), Some(false))
    );
    // A key is never accepted as an argument, and api-key mode needs a stored key.
    assert!(
        !f.run(&["account", "auth", id, "--key", "AIzaARGV"])
            .status
            .success()
    );
    assert!(
        f.api_error(&["account", "auth", id, "--mode", "api-key"])
            .contains("API_KEY_REQUIRED")
    );
    assert!(
        f.api_error(&["account", "auth", id, "--mode", "vertex"])
            .contains("UNSUPPORTED_AUTH_MODE")
    );

    // 1. An invalid key through the human CLI: stored, validated once, auth_failed.
    let bad = "AIzaBAD-fixture-secret-0001";
    let (ok, out, err) = with_stdin(
        &f,
        &["account", "auth", id, "--mode", "api-key", "--key-stdin"],
        &format!("{bad}\n"),
    );
    assert!(ok, "{err}");
    assert!(!out.contains(bad) && !err.contains(bad), "{out}{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["auth_mode"], "api_key");
    assert_eq!(v["api_key_present"], true);
    assert_eq!(v["validation"]["state"], "auth_failed", "{v}");
    assert_eq!(v["validation"]["code"], "AUTH_API_KEY_INVALID");
    assert_eq!(v["auth_failed"], true);
    assert_eq!(v["auth_required"], true);
    assert!(
        v["auth_failure_reason"]
            .as_str()
            .unwrap()
            .starts_with("AUTH_API_KEY_INVALID:")
    );
    let key_file = home.join("secrets/gemini-api-key");
    assert_eq!(read(&key_file), bad);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        (mode(&home.join("secrets")), mode(&key_file)),
        (0o700, 0o600)
    );
    let settings: Value = serde_json::from_str(&read(&home.join(".gemini/settings.json"))).unwrap();
    assert_eq!(
        settings["security"]["auth"]["selectedType"],
        "gemini-api-key"
    );
    // The validation child got the key only through the environment, in API-key mode,
    // in a throwaway home that is removed afterwards.
    let report = read(
        &f.paths
            .root
            .join(sessionsation::layout::PROVIDER_STATE)
            .join("fake-validate-report"),
    );
    assert!(
        report.contains("selected_type=gemini-api-key\n"),
        "{report}"
    );
    assert!(
        report.contains(&format!("key_sha256={:x}\n", Sha256::digest(bad))),
        "{report}"
    );
    assert!(
        report.contains("api_key=present\nwrapper_key=present\n"),
        "{report}"
    );
    assert!(
        report.contains("trust=true\n")
            && report.contains("args=-p  -o json --approval-mode default\n"),
        "{report}"
    );
    assert!(!report.contains(bad));
    assert_eq!(
        std::fs::read_dir(
            f.paths
                .root
                .join(sessionsation::layout::PROVIDER_STATE)
                .join("gemini-validate")
        )
        .unwrap()
        .count(),
        0
    );
    let row = f.api(&["account", "list", "--platform", "gemini"])[0].clone();
    assert_eq!(
        (
            row["auth_mode"].as_str(),
            row["api_key_present"].as_bool(),
            row["auth_failed"].as_bool(),
            row["selectable"].as_bool()
        ),
        (Some("api_key"), Some(true), Some(true), Some(false)),
        "{row}"
    );
    // An interactive login does not apply to an API-key account.
    let login = f.run(&["account", "login", id]);
    assert!(!login.status.success());
    assert!(String::from_utf8_lossy(&login.stderr).contains("AUTH_MODE_API_KEY"));

    // 2. A valid key through the public API (JSON on stdin) replaces it and clears the failure.
    let good = "AIzaVALID-fixture-secret-0002";
    let (ok, out, err) = with_stdin(
        &f,
        &[
            "api",
            "account",
            "auth",
            id,
            "--mode",
            "api-key",
            "--key-stdin",
        ],
        &json!({"api_key": good}).to_string(),
    );
    assert!(ok, "{err}");
    assert!(!out.contains(good) && !err.contains(good));
    let v: Value = serde_json::from_str::<Value>(&out).unwrap()["result"].clone();
    assert_eq!(v["validation"]["state"], "valid", "{v}");
    assert_eq!(
        (v["auth_failed"].as_bool(), v["auth_required"].as_bool()),
        (Some(false), Some(false)),
        "{v}"
    );
    assert_eq!(read(&key_file), good);
    // Malformed input never quotes the key back.
    let (ok, out, err) = with_stdin(
        &f,
        &[
            "api",
            "account",
            "auth",
            id,
            "--mode",
            "api-key",
            "--key-stdin",
        ],
        &format!("{{\"api_key\":\"{good}\",\"extra\":1}}"),
    );
    assert!(!ok && !out.contains(good) && !err.contains(good), "{err}");
    let (ok, _, err) = with_stdin(
        &f,
        &[
            "api",
            "account",
            "auth",
            id,
            "--mode",
            "api-key",
            "--key-stdin",
        ],
        &json!({"api_key": format!("{good} with space")}).to_string(),
    );
    assert!(
        !ok && err.contains("INVALID_API_KEY_FORMAT") && !err.contains(good),
        "{err}"
    );
    assert_eq!(read(&key_file), good, "a refused key leaves the stored one");

    // 3. Headless delivery of this account receives the stored key (env only, never argv);
    // an ambient key in the caller's environment is still stripped.
    let (session, _native) = f.stopped_session("keyed");
    let message = uuid::Uuid::new_v4().to_string();
    f.send_ok(&resume_body(&message, &f.session(&session), "hello"));
    let settled = f.settled(&message);
    assert_eq!(settled["state"], "completed_with_reply", "{settled}");
    let launch = read(&home.join("fake-print-resume"));
    assert!(
        launch.contains("api_key=present\nwrapper_key=present\n"),
        "{launch}"
    );
    assert!(!launch.contains(good) && !launch.contains("key-must-not-reach-gemini"));

    // Nothing under the data root other than the key file holds either key.
    let mut files = vec![];
    every_file_text(&f.paths.root, &key_file, &mut files);
    for (path, text) in &files {
        assert!(
            !text.contains(good) && !text.contains(bad),
            "key leaked into {}",
            path.display()
        );
    }

    // 4. Back to Google login: the key stays stored but unused; the login plan is returned.
    let v = f.api(&["account", "auth", id, "--mode", "google"]);
    assert_eq!(v["auth_mode"], "google");
    assert_eq!(v["changed"], true);
    assert_eq!(v["login"]["action"], "exec");
    assert_eq!(v["api_key_present"], true);
    let settings: Value = serde_json::from_str(&read(&home.join(".gemini/settings.json"))).unwrap();
    assert_eq!(
        settings["security"]["auth"]["selectedType"],
        "oauth-personal"
    );
    let message = uuid::Uuid::new_v4().to_string();
    f.send_ok(&resume_body(&message, &f.session(&session), "again"));
    assert_eq!(f.settled(&message)["state"], "completed_with_reply");
    let launch = read(&home.join("fake-print-resume"));
    assert!(
        launch.contains("api_key=\nwrapper_key=\n"),
        "google mode passes no key: {launch}"
    );

    // 5. Logout of an API-key account removes the key.
    f.api(&["account", "auth", id, "--mode", "api-key"]);
    let out = f.api(&["account", "logout", id]);
    assert_eq!(out["auth_required"], true);
    assert!(!key_file.exists());
    let status = f.api(&["account", "auth", id]);
    assert_eq!(
        (
            status["auth_mode"].as_str(),
            status["api_key_present"].as_bool(),
            status["auth_required"].as_bool()
        ),
        (Some("api_key"), Some(false), Some(true))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_last_accessed_is_the_last_chat_record_and_null_without_a_source() {
    let f = fixture().await;
    f.add_logged_in("work");
    let (_, native) = f.stopped_session("work");
    let path = find_chat(&f.home("work"), &native);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"gemini","timestamp":"2026-10-29T10:05:00.000Z","content":"x"})
        )
        .unwrap();
    }
    let s = f.by_conversation(&native);
    assert_eq!(s["last_accessed"], 1_793_268_300_000_i64, "{s}");
    std::fs::remove_file(&path).unwrap();
    if let Some(s) = f
        .gemini_sessions()
        .into_iter()
        .find(|s| s["conversation_id"] == native.as_str())
    {
        assert!(s["last_accessed"].is_null(), "{s}");
    }
}
