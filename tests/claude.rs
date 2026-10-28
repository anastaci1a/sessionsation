//! Claude provider integration with a metadata-only fake (`support/fake-claude.sh`).
//! Disposable homes only; no provider network, credentials or model requests.
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
    fn api_error(&self, args: &[&str]) -> String {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
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
    fn session(&self, id: &str) -> Value {
        self.claude_sessions()
            .into_iter()
            .find(|s| s["id"] == id)
            .unwrap()
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("claude-accounts").join(name)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrollment_requires_a_platform_and_names_are_global_across_providers() {
    let f = fixture().await;
    // --platform is required and invalid values list the choices; no Codex default.
    let missing = f.run(&["account", "add", "work"]);
    assert_eq!(missing.status.code(), Some(2));
    let text = String::from_utf8_lossy(&missing.stderr);
    assert!(
        text.contains("--platform") && text.contains("codex|claude"),
        "{text}"
    );
    let invalid = f.run(&["account", "add", "--platform", "gemini", "work"]);
    assert_eq!(invalid.status.code(), Some(2));
    let text = String::from_utf8_lossy(&invalid.stderr);
    assert!(text.contains("codex") && text.contains("claude"), "{text}");
    assert!(!f.paths.root.join("accounts/work").exists());

    let add = f.run(&["account", "add", "--platform", "claude", "personal"]);
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let home = f.home("personal");
    assert_eq!(
        std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let add = f.run(&["account", "add", "--platform", "codex", "work"]);
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    // Collision across providers names the existing owner.
    let clash = f.run(&["account", "add", "--platform", "codex", "personal"]);
    let text = String::from_utf8_lossy(&clash.stderr);
    assert!(!clash.status.success());
    assert!(
        text.contains("ACCOUNT_NAME_EXISTS") && text.contains("claude:personal"),
        "{text}"
    );
    assert!(!f.paths.root.join("accounts/personal").exists());
    let text = f.api_error(&["account", "add", "claude:work"]);
    assert!(
        text.contains("ACCOUNT_PROVIDER_MISMATCH") || text.contains("ACCOUNT_NAME_EXISTS"),
        "{text}"
    );
    // API ids must be qualified and must match the enrolled provider.
    assert!(
        f.api_error(&["account", "add", "other"])
            .contains("ACCOUNT_PROVIDER_REQUIRED")
    );
    let text = f.api_error(&["account", "disable", "claude:work"]);
    assert!(
        text.contains("ACCOUNT_PROVIDER_MISMATCH") && text.contains("codex:work"),
        "{text}"
    );
    let text = f.api_error(&["account", "remove", "claude:personal"]);
    assert!(text.contains("UNSUPPORTED_PROVIDER_OPERATION"), "{text}");
    assert!(
        f.api_error(&["account", "add", "codex:x", "--config-dir", "/tmp"])
            .contains("INVALID_ARGUMENT")
    );

    // Adoption validates the directory and never copies it.
    let relative = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--config-dir",
        "relative",
        "a1",
    ]);
    assert!(String::from_utf8_lossy(&relative.stderr).contains("INVALID_CONFIG_DIR"));
    let inside = f.paths.root.join("accounts/work");
    let nested = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--config-dir",
        inside.to_str().unwrap(),
        "a2",
    ]);
    assert!(String::from_utf8_lossy(&nested.stderr).contains("INVALID_CONFIG_DIR"));
    let shared = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--config-dir",
        home.to_str().unwrap(),
        "a3",
    ]);
    assert!(!shared.status.success());
    let adopted = f._tmp.path().join("existing-claude");
    std::fs::create_dir(&adopted).unwrap();
    std::fs::write(adopted.join("marker"), b"kept").unwrap();
    let adopt = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--config-dir",
        adopted.to_str().unwrap(),
        "adopted",
    ]);
    assert!(
        adopt.status.success(),
        "{}",
        String::from_utf8_lossy(&adopt.stderr)
    );
    assert!(!f.home("adopted").exists(), "adopted homes are not copied");
    assert_eq!(read(&adopted.join("marker")), "kept");
    let reuse = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--config-dir",
        adopted.to_str().unwrap(),
        "again",
    ]);
    assert!(String::from_utf8_lossy(&reuse.stderr).contains("CONFIG_DIR_IN_USE"));

    // Unified listing with provider, filter and qualified ids.
    let all = f.api(&["account", "list"]);
    let ids: Vec<_> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        ids.contains(&"claude:personal".into())
            && ids.contains(&"codex:work".into())
            && ids.contains(&"claude:adopted".into()),
        "{ids:?}"
    );
    let claude_only = f.api(&["account", "list", "--platform", "claude"]);
    assert!(
        claude_only
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["provider"] == "claude")
    );
    let adopted_row = claude_only
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "adopted")
        .unwrap()
        .clone();
    assert_eq!(
        adopted_row["config_home"],
        json!(std::fs::canonicalize(&adopted).unwrap())
    );
    let table = f.run(&["account", "list"]);
    let table = String::from_utf8_lossy(&table.stdout);
    assert!(
        table.contains("PROVIDER") && table.contains("claude") && table.contains("codex"),
        "{table}"
    );
    assert!(
        !all.to_string().contains("example.invalid"),
        "auth identity must not be exposed"
    );

    // Interactive login runs the official subscription login with the account env.
    let login = f.run(&["account", "login", "claude:adopted"]);
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stderr)
    );
    assert_eq!(
        read(&adopted.join("fake-login-args")).trim(),
        "auth login --claudeai"
    );
    let plan = f.api(&["account", "login", "claude:adopted"]);
    assert_eq!(plan["login"]["action"], "exec");
    assert!(
        plan["login"]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "claude:adopted")
    );
    let caps = f.api(&["capabilities"]);
    let claude = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "claude")
        .unwrap()
        .clone();
    assert_eq!(claude["implemented"], true);
    assert_eq!(claude["operations"]["submit"]["supported"], false);
    assert_eq!(caps["providers"][0]["steer"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_sessions_launch_track_live_identity_rename_and_resume() {
    let f = fixture().await;
    assert!(
        f.run(&["account", "add", "--platform", "claude", "personal"])
            .status
            .success()
    );
    let home = f.home("personal");
    wait_for("auth status poll", || {
        f.api(&["account", "list"])[0]["auth_required"] == false
    });
    let launcher = f.api(&["launcher", "new", "--account", "claude:personal"]);
    let argv: Vec<_> = launcher["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert!(
        argv.ends_with(&["run".into(), "--account".into(), "claude:personal".into()]),
        "{argv:?}"
    );

    // Wrong auth modes are refused before any session is recorded.
    for (mode, code) in [
        ("api_key", "WRONG_AUTH_MODE"),
        ("bedrock", "WRONG_AUTH_MODE"),
        ("logged_out", "AUTH_REQUIRED"),
    ] {
        std::fs::write(home.join("fake-auth-mode"), mode).unwrap();
        let out = f
            .command(&["run", "--account", "claude:personal"])
            .current_dir(&f.project)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && text.contains(code),
            "{mode}: {text}"
        );
        assert!(!text.contains("example.invalid"));
    }
    assert!(f.claude_sessions().is_empty());
    std::fs::remove_file(home.join("fake-auth-mode")).unwrap();

    // New launch: ambient API credentials are stripped; native id is recorded first.
    let child = f
        .command(&["run", "--account", "personal"])
        .current_dir(&f.project)
        .env("ANTHROPIC_API_KEY", "sk-test-must-not-reach-claude")
        .env("ANTHROPIC_BASE_URL", "https://example.invalid")
        .spawn()
        .unwrap();
    wait_for("fake claude launch", || {
        home.join("fake-launch-new").exists()
            && std::fs::read_dir(home.join("sessions")).is_ok_and(|d| d.count() >= 2)
    });
    let launch = read(&home.join("fake-launch-new"));
    assert!(
        launch.contains("api_key=\n") && launch.contains("base_url=\n"),
        "{launch}"
    );
    assert!(launch.contains("autoupdater=1"), "{launch}");
    assert!(
        launch.contains(&format!("config_dir={}", home.display())),
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
    let sessions = f.claude_sessions();
    assert_eq!(
        sessions.len(),
        1,
        "launch and transcript discovery are one session"
    );
    let s = &sessions[0];
    assert_eq!(s["conversation_id"], native);
    assert_eq!(s["conversation_id_source"], "native");
    assert_eq!(s["account_id"], "claude:personal");
    assert_eq!(s["availability"], "running");
    assert_eq!(s["native_status"], "busy");
    assert_eq!(s["native_name"], s["name"]);
    assert_eq!(s["pid"], child.id());
    assert_eq!(s["directory"], json!(f.project));
    assert_eq!(s["capabilities"]["resume"], false);
    assert_eq!(s["capabilities"]["submit"], false);
    assert!(
        s["capability_reasons"]["switch_account"]
            .as_str()
            .unwrap()
            .contains("config home")
    );
    assert!(
        !f.api(&["state"])
            .to_string()
            .contains("fixture prompt text"),
        "transcript text must never surface"
    );
    let id = s["id"].as_str().unwrap().to_owned();
    let details = f.api(&[
        "session",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
    ]);
    assert_eq!(details["provider"], "claude");
    assert_eq!(details["availability"], "running");

    // Live sessions are not resumed again.
    let generation = s["generation"].as_str().unwrap().to_owned();
    let out = f.run(&["resume", &id, "--expected-generation", &generation]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("SESSION_ALREADY_RUNNING"));
    assert!(
        f.api_error(&[
            "launcher",
            "resume",
            "--session",
            &id,
            "--expected-generation",
            &generation
        ])
        .contains("SESSION_ALREADY_RUNNING")
    );
    // Unsupported operations are rejected by provider, not attempted.
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

    // Exited process with a leftover registry entry: stopped, not live.
    let s = f.session(&id);
    assert_eq!(s["availability"], "stopped");
    assert_eq!(s["capabilities"]["resume"], true);
    // Without a usable saved directory, resume is declared unavailable up front.
    let away = f.project.with_extension("away");
    std::fs::rename(&f.project, &away).unwrap();
    let gone = f.session(&id);
    std::fs::rename(&away, &f.project).unwrap();
    assert_eq!(gone["capabilities"]["resume"], false);
    assert_eq!(
        gone["capability_reasons"]["resume"],
        "SAVED_DIRECTORY_UNAVAILABLE"
    );
    assert!(s["pid"].is_null());

    // Catalog rename: identity unchanged, label persisted, generation checked.
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
    assert_eq!(s["account_id"], "claude:personal");
    let catalog = read(&f.paths.root.join("session-catalog/catalog.json"));
    assert!(
        catalog.contains("relabeled"),
        "rename persists in the catalog file"
    );

    // Launcher plan resumes in the recorded directory; exec resumes with the label.
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
    wait_for("fake claude resume", || {
        home.join("fake-launch-resume").exists()
    });
    let resume = read(&home.join("fake-launch-resume"));
    assert!(resume.contains(&format!("session={native}")), "{resume}");
    assert!(resume.contains("name=relabeled"), "{resume}");
    assert!(
        resume.contains(&format!("cwd={}", f.project.display())),
        "{resume}"
    );
    wait_for("resumed session live", || {
        f.session(&id)["availability"] == "running"
    });
    kill(child);

    // Missing transcript: stale, and resume refuses.
    let transcript = std::fs::read_dir(home.join("projects"))
        .unwrap()
        .flatten()
        .map(|d| d.path().join(format!("{native}.jsonl")))
        .find(|p| p.exists())
        .unwrap();
    std::fs::remove_file(transcript).unwrap();
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
async fn statusline_usage_is_recorded_chained_and_projected_without_zero_fill() {
    let f = fixture().await;
    assert!(
        f.run(&["account", "add", "--platform", "claude", "personal"])
            .status
            .success()
    );
    let home = f.home("personal");
    let account = |f: &Fixture| f.api(&["account", "list", "--platform", "claude"])[0].clone();
    let before = account(&f);
    assert_eq!(before["usage_fresh"], false);
    assert_eq!(before["usage_state"], "missing");
    assert!(before["usage"]["short"].is_null() && before["usage"]["weekly"].is_null());
    std::fs::write(
        home.join("settings.json"),
        json!({"statusLine":{"type":"command","command":"cat >/dev/null; printf chained-line"}})
            .to_string(),
    )
    .unwrap();
    let now = daemon::now();
    let hook = |input: Value, record_only: bool| {
        let mut args = vec!["provider-hook", "claude-statusline"];
        if record_only {
            args.push("--record-only");
        }
        let mut child = Command::new(binary())
            .args(&args)
            .env("CLAUDE_CONFIG_DIR", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let out = hook(
        json!({"session_id":"s","version":"2.1.312","rate_limits":{"five_hour":{"used_percentage":12.5,"resets_at":now+3600},"seven_day":{"used_percentage":40,"resets_at":now+86400}}}),
        false,
    );
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "chained-line");
    let a = account(&f);
    assert_eq!(a["usage_fresh"], true);
    assert_eq!(a["usage"]["short"]["used_percent"], 12.5);
    assert_eq!(a["usage"]["short"]["duration_minutes"], 300);
    assert_eq!(a["usage"]["weekly"]["duration_minutes"], 10080);
    assert_eq!(a["usage"]["source"], "claude-code-statusline");
    let out = hook(
        json!({"rate_limits":{"seven_day":{"used_percentage":41,"resets_at":now+86400}}}),
        true,
    );
    assert!(out.stdout.is_empty(), "--record-only does not chain");
    let a = account(&f);
    assert_eq!(a["usage_state"], "partial");
    assert_eq!(a["usage_fresh"], false);
    assert!(
        a["usage"]["short"].is_null(),
        "a missing window is null, never 0%"
    );
    // Broken input never fails the statusline.
    let mut child = Command::new(binary())
        .args(["provider-hook", "claude-statusline"])
        .env("CLAUDE_CONFIG_DIR", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"not json").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "chained-line");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_home_prepopulated_by_an_operator_is_registered_without_clobbering() {
    let f = fixture().await;
    let prepare = |name: &str, mode: u32| {
        let home = f.home(name);
        std::fs::create_dir_all(home.join("projects")).unwrap();
        std::fs::write(home.join("settings.json"), b"{\"theme\":\"kept\"}").unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(mode)).unwrap();
        home
    };
    let migrated = prepare("migrated", 0o700);
    let out = f.run(&["account", "add", "--platform", "claude", "migrated"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("registered existing") && text.contains("Auth: logged in"),
        "{text}"
    );
    assert!(!text.contains("example.invalid"));
    assert_eq!(
        read(&migrated.join("settings.json")),
        "{\"theme\":\"kept\"}"
    );
    assert!(migrated.join("projects").is_dir());
    let row = f.api(&["account", "list", "--platform", "claude"])[0].clone();
    assert_eq!(row["id"], "claude:migrated");
    assert!(
        row["config_home"].is_null(),
        "managed layout, not an adoption"
    );
    assert_eq!(row["auth_required"], false);

    // A home that is not private is refused and left exactly as it was.
    let loose = prepare("loose", 0o755);
    let out = f.run(&["account", "add", "--platform", "claude", "loose"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("UNSAFE_ACCOUNT_HOME"));
    assert_eq!(
        std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(read(&loose.join("settings.json")), "{\"theme\":\"kept\"}");
    assert_eq!(f.api(&["account", "list"]).as_array().unwrap().len(), 1);

    // The API reports the existing home's auth state (logged out here).
    let pending = prepare("pending", 0o700);
    std::fs::write(pending.join("fake-auth-mode"), "logged_out").unwrap();
    let added = f.api(&["account", "add", "claude:pending"]);
    assert_eq!(added["existing_home"], true);
    assert_eq!(added["auth"]["problem"], "AUTH_REQUIRED");
    assert_eq!(added["login"]["action"], "exec");
    assert_eq!(read(&pending.join("settings.json")), "{\"theme\":\"kept\"}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_home_switches_to_the_managed_home_only_when_every_precondition_holds() {
    let f = fixture().await;
    let adopted = f._tmp.path().join("adopted");
    std::fs::create_dir(&adopted).unwrap();
    std::fs::write(adopted.join("settings.json"), b"{\"theme\":\"kept\"}").unwrap();
    let add = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--config-dir",
        adopted.to_str().unwrap(),
        "moved",
    ]);
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let row = || f.api(&["account", "list", "--platform", "claude"])[0].clone();
    assert!(!row()["config_home"].is_null());
    let events = || -> Vec<String> {
        let db = rusqlite::Connection::open(f.paths.root.join("registry.db")).unwrap();
        db.prepare("SELECT entity FROM events WHERE kind='account.claude_home_relocated'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };

    // A codex account is refused with the provider error.
    let add = f.run(&["account", "add", "--platform", "codex", "cx"]);
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let codex = f.api_error(&["account", "managed-home", "codex:cx"]);
    assert!(codex.contains("UNSUPPORTED_PROVIDER_OPERATION"), "{codex}");
    let mismatch = f.api_error(&["account", "managed-home", "codex:moved"]);
    assert!(mismatch.contains("ACCOUNT_PROVIDER_MISMATCH"), "{mismatch}");

    // Missing managed dir.
    let managed = f.home("moved");
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    assert!(err.contains("ACCOUNT_HOME_UNAVAILABLE"), "{err}");
    assert!(!managed.exists(), "never created");

    // Unsafe managed dir (group-readable), left unchanged.
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    assert!(err.contains("UNSAFE_ACCOUNT_HOME"), "{err}");
    assert_eq!(
        std::fs::metadata(&managed).unwrap().permissions().mode() & 0o777,
        0o755
    );
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o700)).unwrap();

    // A symlinked managed dir is refused.
    let real = f._tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir(&managed).unwrap();
    std::os::unix::fs::symlink(&real, &managed).unwrap();
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    assert!(err.contains("UNSAFE_ACCOUNT_HOME"), "{err}");
    std::fs::remove_file(&managed).unwrap();
    std::fs::create_dir(&managed).unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Logged-out managed home, then another identity.
    std::fs::write(managed.join("fake-auth-mode"), "logged_out").unwrap();
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    assert!(err.contains("AUTH_REQUIRED"), "{err}");
    std::fs::write(managed.join("fake-auth-mode"), "api_key").unwrap();
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    assert!(err.contains("WRONG_AUTH_MODE"), "{err}");
    std::fs::write(managed.join("fake-auth-mode"), "subscription").unwrap();
    std::fs::write(managed.join("fake-identity"), "someone-else").unwrap();
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    assert!(err.contains("AUTH_IDENTITY_MISMATCH"), "{err}");
    assert!(!err.contains("example.invalid") && !err.contains("someone-else"));
    std::fs::remove_file(managed.join("fake-identity")).unwrap();

    // Any process whose environment names either home blocks the switch.
    let holder = Command::new("sleep")
        .arg("60")
        .env("CLAUDE_CONFIG_DIR", &managed)
        .spawn()
        .unwrap();
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    kill(holder);
    assert!(err.contains("ACCOUNT_IN_USE"), "{err}");
    // So does a verified live Claude session on the adopted home.
    let fake = f._tmp.path().join("fake-claude");
    let live = Command::new(&fake)
        .args(["--session-id", "5b0f4b9e-6f1b-4c36-9d0a-1b6f3a1d2c11"])
        .env("CLAUDE_CONFIG_DIR", &adopted)
        .current_dir(&f.project)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("live registry entry", || {
        std::fs::read_dir(adopted.join("sessions")).is_ok_and(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".json"))
        })
    });
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    kill(live);
    // The fake's orphaned `sleep 1` child still carries the environment briefly.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(err.contains("ACCOUNT_IN_USE"), "{err}");
    assert!(events().is_empty());
    assert!(
        !row()["config_home"].is_null(),
        "nothing changed on refusal"
    );

    // Success through the human CLI; id, name and catalog are unchanged.
    let sessions_before = f.claude_sessions();
    let out = f.run(&["account", "managed-home", "moved"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("\"relocated\": true") && !text.contains("example.invalid"),
        "{text}"
    );
    let r = row();
    assert_eq!(r["id"], "claude:moved");
    assert!(r["config_home"].is_null());
    assert_eq!(f.claude_sessions().len(), sessions_before.len());
    assert_eq!(events(), vec!["moved".to_owned()]);
    assert_eq!(read(&adopted.join("settings.json")), "{\"theme\":\"kept\"}");

    // Idempotent via the API: no second event.
    let again = f.api(&["account", "managed-home", "claude:moved"]);
    assert_eq!(again["already_managed"], true);
    assert_eq!(again["note"], "ALREADY_MANAGED");
    assert_eq!(events().len(), 1);
}
