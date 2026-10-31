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
async fn enrollment_requires_a_platform_and_names_are_unique_per_provider() {
    let f = fixture().await;
    // --platform is required and invalid values list the choices; no Codex default.
    let missing = f.run(&["account", "add", "work"]);
    assert_eq!(missing.status.code(), Some(2));
    let text = String::from_utf8_lossy(&missing.stderr);
    assert!(
        text.contains("--platform") && text.contains("codex|claude|gemini"),
        "{text}"
    );
    let invalid = f.run(&["account", "add", "--platform", "gpt", "work"]);
    assert_eq!(invalid.status.code(), Some(2));
    let text = String::from_utf8_lossy(&invalid.stderr);
    assert!(
        text.contains("codex") && text.contains("claude") && text.contains("gemini"),
        "{text}"
    );
    assert!(!f.paths.root.join("accounts/codex/work").exists());

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
    // Names are unique per provider: another provider may reuse a name, the
    // same provider may not, and the error names the existing owner.
    let other = f.run(&["account", "add", "--platform", "codex", "personal"]);
    assert!(
        other.status.success(),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
    assert!(f.paths.root.join("accounts/codex/personal").exists());
    let clash = f.run(&["account", "add", "--platform", "codex", "personal"]);
    let text = String::from_utf8_lossy(&clash.stderr);
    assert!(!clash.status.success());
    assert!(
        text.contains("ACCOUNT_NAME_EXISTS") && text.contains("codex:personal"),
        "{text}"
    );
    let text = f.api_error(&["account", "add", "codex:work"]);
    assert!(text.contains("ACCOUNT_NAME_EXISTS"), "{text}");
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
    let preview = f.api(&["account", "remove", "claude:personal"]);
    assert_eq!(preview["account_id"], "claude:personal");
    assert!(
        f.api_error(&["account", "remove", "gemini:personal"])
            .contains("ACCOUNT_PROVIDER_MISMATCH")
    );
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
    let inside = f.paths.root.join("accounts/codex/work");
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
    // Revision 16: supported per session (hosted, or live with live steering enabled).
    assert_eq!(claude["operations"]["submit"]["supported"], true);
    assert_eq!(claude["input_delivery"]["hosted"]["implicit_bypass"], false);
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

    // Revision 20: a yolo start pre-accepts Claude's folder trust in .claude.json.
    std::fs::write(home.join(".claude.json"), "{\"numStartups\": 3}").unwrap();
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
    let config: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
    let trusted = std::fs::canonicalize(&f.project).unwrap();
    assert_eq!(config["numStartups"], 3, "{config}");
    assert_eq!(
        config["projects"][trusted.to_str().unwrap()]["hasTrustDialogAccepted"],
        true,
        "{config}"
    );
    let launch = read(&home.join("fake-launch-new"));
    assert!(
        launch.contains("api_key=\n") && launch.contains("base_url=\n"),
        "{launch}"
    );
    // Launch permissions default to yolo: bypass flag plus the root sandbox opt-in.
    assert!(
        launch.contains("bypass=1\n") && launch.contains("is_sandbox=1\n"),
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
        s["capability_reasons"]["steer"]
            .as_str()
            .unwrap()
            .starts_with("LIVE_STEERING_DISABLED")
    );
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
    assert_eq!(
        renamed["name_source"], "catalog",
        "detached sessions keep the catalog label"
    );
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
    let catalog = read(
        &f.paths
            .root
            .join(sessionsation::layout::SESSION_CATALOG)
            .join("catalog.json"),
    );
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
    // An explicit `--permissions default` resume starts Claude without bypass.
    // ... and leaves folder trust alone.
    std::fs::write(home.join(".claude.json"), "{\"numStartups\": 4}").unwrap();
    let child = f
        .command(&[
            "resume",
            &id,
            "--expected-generation",
            &generation,
            "--permissions",
            "default",
        ])
        .current_dir("/")
        .spawn()
        .unwrap();
    wait_for("fake claude resume", || {
        home.join("fake-launch-resume").exists()
    });
    let resume = read(&home.join("fake-launch-resume"));
    assert!(resume.contains(&format!("session={native}")), "{resume}");
    assert!(
        resume.contains("bypass=\n") && resume.contains("is_sandbox=\n"),
        "{resume}"
    );
    assert!(resume.contains("name=relabeled"), "{resume}");
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        "{\"numStartups\": 4}"
    );
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
        let db = rusqlite::Connection::open(f.paths.root.join(sessionsation::layout::REGISTRY_DB))
            .unwrap();
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
    // A non-Claude process that merely inherited CLAUDE_CONFIG_DIR (e.g. a provider
    // server started from a Claude shell) does not trip the in-use check: the switch
    // proceeds to (and is refused by) the identity check instead.
    let bystander = Command::new("sleep")
        .arg("60")
        .env("CLAUDE_CONFIG_DIR", &managed)
        .spawn()
        .unwrap();
    let err = f.api_error(&["account", "managed-home", "claude:moved"]);
    kill(bystander);
    assert!(err.contains("AUTH_IDENTITY_MISMATCH"), "{err}");
    std::fs::remove_file(managed.join("fake-identity")).unwrap();

    // ...but a process started as the configured Claude command does.
    let holder = Command::new("bash")
        .arg("-c")
        .arg("exec -a \"$0\" sleep 60")
        .arg(f._tmp.path().join("fake-claude"))
        .env("CLAUDE_CONFIG_DIR", &managed)
        .spawn()
        .unwrap();
    let holder_cmdline = format!("/proc/{}/cmdline", holder.id());
    let fake_path = f._tmp.path().join("fake-claude");
    wait_for("holder exec as the Claude command", || {
        std::fs::read(&holder_cmdline).is_ok_and(|c| {
            c.split(|b| *b == 0).next() == Some(fake_path.as_os_str().as_encoded_bytes())
        })
    });
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
    // The fake's orphaned `sleep 1` child still carries the environment until it
    // exits; wait for that rather than a fixed delay (slow under load).
    wait_for("orphaned child to release the adopted home", || {
        !std::fs::read_dir("/proc").unwrap().flatten().any(|e| {
            std::fs::read(e.path().join("environ")).is_ok_and(|env| {
                let mut want = b"CLAUDE_CONFIG_DIR=".to_vec();
                want.extend_from_slice(adopted.as_os_str().as_encoded_bytes());
                env.split(|b| *b == 0).any(|v| v == want.as_slice())
            })
        })
    });
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

// ------------------------------------------------- lifecycle, login, headless

fn now() -> i64 {
    daemon::now()
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
    fn send(&self, body: &Value) -> Output {
        let mut child = self
            .command(&["api", "send"])
            .env("ANTHROPIC_API_KEY", "sk-test-must-not-reach-claude")
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
        // The worker must have released its lock before another action starts.
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
}
fn resume_body(id: &str, session: &Value, text: &str) -> Value {
    json!({"api_version":1,"message_id":id,"mode":"resume","session_id":session["id"],
        "expected_generation":session["generation"],"text":text,"expires_at":now()+600})
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
    assert_eq!(first["provider"], "claude");
    assert_eq!(first["delivery_semantics"], "headless_print");
    let r = f.settled(&m1);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["reply"]["text"], "fake reply to: hello world");
    assert_eq!(r["reply"]["session_id"], native.as_str());
    assert_eq!(r["reply"]["num_turns"], 1);
    assert_eq!(r["reply"]["usage"]["output_tokens"], 4);
    assert_eq!(r["correlation"]["matched"], true);
    assert_eq!(r["facts"]["reply_observed"], true);
    assert_eq!(r["session_id"], id.as_str());
    assert_eq!(r["provider_conversation_id"], native.as_str());
    let launch = read(&home.join("fake-print-resume"));
    assert!(launch.contains(&format!("session={native}")), "{launch}");
    assert!(
        launch.contains(&format!("cwd={}", f.project.display())),
        "{launch}"
    );
    assert!(launch.contains("message=hello world"), "{launch}");
    assert!(
        launch.contains("api_key=\n") && launch.contains("autoupdater=1"),
        "{launch}"
    );
    assert!(
        launch.contains(&format!("config_dir={}", home.display())),
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
    std::fs::write(home.join("fake-auth-mode"), "logged_out").unwrap();
    let session = f.session(&id);
    let refused = uuid::Uuid::new_v4().to_string();
    let err = f.send_err(&resume_body(&refused, &session, "x"));
    assert!(err.contains("AUTH_REQUIRED"), "{err}");
    std::fs::remove_file(home.join("fake-auth-mode")).unwrap();
    assert!(
        f.api_error(&["receipt", &refused])
            .contains("receipt_not_found")
    );

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

    // Stale (transcript missing) and live sessions are refused before admission.
    let (stale, stale_native) = f.stopped_session("personal");
    let transcript = std::fs::read_dir(home.join("projects"))
        .unwrap()
        .flatten()
        .map(|d| d.path().join(format!("{stale_native}.jsonl")))
        .find(|p| p.exists())
        .unwrap();
    std::fs::remove_file(transcript).unwrap();
    let s = f.session(&stale);
    assert_eq!(s["availability"], "stale");
    let err = f.send_err(&resume_body(&uuid::Uuid::new_v4().to_string(), &s, "x"));
    assert!(err.contains("SESSION_STALE"), "{err}");
    let live_native = uuid::Uuid::new_v4().to_string();
    let live = Command::new(f._tmp.path().join("fake-claude"))
        .args(["--session-id", &live_native])
        .env("CLAUDE_CONFIG_DIR", &home)
        .current_dir(&f.project)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut live_session = Value::Null;
    wait_for("live session", || {
        live_session = f
            .claude_sessions()
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
    kill(live);
    assert!(err.contains("SESSION_ALREADY_RUNNING"), "{err}");

    // Submit to a stopped session is refused with a reason (revision 16: hosted or live only).
    let mut submit = resume_body(&uuid::Uuid::new_v4().to_string(), &f.session(&id), "x");
    submit["mode"] = json!("submit");
    let err = f.send_err(&submit);
    assert!(
        !err.contains("UNSUPPORTED_PROVIDER_OPERATION") && err.contains("SESSION_NOT_RUNNING"),
        "{err}"
    );

    // New conversation: catalog entry first, pre-assigned --session-id.
    let created = uuid::Uuid::new_v4().to_string();
    let new = json!({"api_version":1,"message_id":created,"mode":"new","account_id":"claude:personal",
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
    let s = f.session(&catalog_id);
    assert_eq!(s["name"], "dm-new");
    assert_eq!(s["conversation_id"], conversation.as_str());
    assert_eq!(s["availability"], "stopped");
    let mut bad = new.clone();
    bad["message_id"] = json!(uuid::Uuid::new_v4().to_string());
    bad["directory"] = json!("relative");
    assert!(f.send_err(&bad).contains("SAVED_DIRECTORY_UNAVAILABLE"));
    assert!(f.api(&["state"]).to_string().find("start here").is_none());

    // Codex contract unchanged: unknown sessions and receipts keep their errors.
    let codex = json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":"submit",
        "session_id":uuid::Uuid::new_v4().to_string(),"expected_generation":"a".repeat(64),"text":"x","expires_at":now()+600});
    assert!(f.send_err(&codex).contains("SESSION_NOT_FOUND"));
    assert!(
        f.api_error(&["receipt", &uuid::Uuid::new_v4().to_string()])
            .contains("receipt_not_found")
    );
    let mut reuse = codex.clone();
    reuse["message_id"] = json!(m1);
    assert!(f.send_err(&reuse).contains("MESSAGE_ID_CONFLICT"));
    let caps = f.api(&["capabilities"]);
    let claude = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "claude")
        .unwrap()
        .clone();
    assert_eq!(claude["operations"]["headless_delivery"]["supported"], true);
    assert_eq!(claude["headless_delivery"]["automatic_replay"], false);
    assert_eq!(
        caps["providers"][0]["operations"]["headless_delivery"]["supported"],
        false
    );
    assert_eq!(caps["contract_revision"], 32);
    assert_eq!(caps["receipt_retention"]["since_revision"], 18);
    assert_eq!(caps["launch_permissions"]["since_revision"], 19);
    assert_eq!(caps["launch_permissions"]["built_in_default"], "yolo");
    assert_eq!(caps["launch_permissions"]["request_field"], "permissions");
    assert_eq!(
        caps["receipt_retention"]["retired_error"],
        "RECEIPT_RETIRED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_claude_home_returns_the_login_plan_and_refresh_observes_login() {
    let f = fixture().await;
    std::fs::create_dir_all(f.paths.root.join("accounts/claude")).unwrap();
    std::fs::set_permissions(
        f.paths.root.join("accounts/claude"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(
        f.paths.root.join("accounts/claude/fake-default-auth"),
        "logged_out",
    )
    .unwrap();
    let added = f.api(&["account", "add", "claude:fresh"]);
    assert_eq!(added["existing_home"], false);
    assert_eq!(added["auth"]["problem"], "AUTH_REQUIRED");
    assert_eq!(added["login_required"], true);
    let plan = f.api(&["account", "login", "claude:fresh"]);
    assert_eq!(
        added["login"], plan["login"],
        "add returns the login command's plan"
    );
    assert_eq!(plan["login"]["credentials_in_client"], false);
    let refreshed = f.api(&["account", "refresh", "claude:fresh"]);
    assert_eq!(refreshed["auth_required"], true);
    assert_eq!(refreshed["auth_detail"], "AUTH_REQUIRED");
    // Execute the plan's argv: the official login, account env, keys stripped.
    let argv: Vec<String> = plan["login"]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env("ANTHROPIC_API_KEY", "sk-test-must-not-reach-claude")
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let home = f.home("fresh");
    assert_eq!(
        read(&home.join("fake-login-args")).trim(),
        "auth login --claudeai"
    );
    let env = read(&home.join("fake-login-env"));
    assert!(
        env.contains("api_key=\n") && env.contains("autoupdater=1"),
        "{env}"
    );
    assert!(
        env.contains(&format!("config_dir={}", home.display())),
        "{env}"
    );
    let refreshed = f.api(&["account", "refresh", "claude:fresh"]);
    assert_eq!(refreshed["auth_required"], false, "{refreshed}");
    assert_eq!(refreshed["auth_mode"]["auth_method"], "claude.ai");
    assert!(!refreshed.to_string().contains("example.invalid"));
    let row = f.api(&["account", "list", "--platform", "claude"])[0].clone();
    assert_eq!(row["auth_required"], false);
    assert_eq!(row["selectable"], true);
    // A logged-in home reports no required login; Codex refresh is explicit.
    std::fs::remove_file(f.paths.root.join("accounts/claude/fake-default-auth")).unwrap();
    let ready = f.api(&["account", "add", "claude:ready"]);
    assert_eq!(ready["login_required"], false);
    assert_eq!(ready["login"]["action"], "exec");
    // The human command without a terminal prints and does not start a login.
    std::fs::write(
        f.paths.root.join("accounts/claude/fake-default-auth"),
        "logged_out",
    )
    .unwrap();
    let out = f.run(&["account", "add", "--platform", "claude", "quiet"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("account login claude:quiet"));
    assert!(!f.home("quiet").join("fake-login-args").exists());
    let add = f.run(&["account", "add", "--platform", "codex", "cx"]);
    assert!(add.status.success());
    assert!(
        f.api_error(&["account", "refresh", "codex:cx"])
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_membership_removal_and_local_deletion_match_codex_semantics() {
    let f = fixture().await;
    f.add_logged_in("gone");
    let home = f.home("gone");
    let (id, native) = f.stopped_session("gone");
    std::fs::write(home.join("history.jsonl"), b"{}\n").unwrap();
    std::fs::create_dir_all(home.join("todos")).unwrap();
    std::fs::write(home.join(".credentials.json"), b"synthetic fixture").unwrap();

    // Membership removal: reversible, hidden from selection, data untouched.
    let preview = f.api(&["account", "remove", "claude:gone"]);
    assert_eq!(preview["can_remove"], true, "{preview}");
    assert_eq!(preview["effects"]["delete_history"], false);
    let revision = preview["revision"].as_str().unwrap().to_owned();
    assert!(
        f.api_error(&[
            "account",
            "remove",
            "claude:gone",
            "--confirm",
            "codex:gone",
            "--revision",
            &revision
        ])
        .contains("CONFIRMATION_REQUIRED")
    );
    let removed = f.api(&[
        "account",
        "remove",
        "claude:gone",
        "--confirm",
        "claude:gone",
        "--revision",
        &revision,
    ]);
    assert_eq!(removed["removed"], true);
    let row = f.api(&["account", "list"])[0].clone();
    assert_eq!(row["membership"], "removed");
    assert_eq!(row["selectable"], false);
    assert!(
        f.api_error(&["launcher", "new", "--account", "claude:gone"])
            .contains("ACCOUNT_REMOVED")
    );
    let s = f.session(&id);
    let err = f.send_err(&resume_body(&uuid::Uuid::new_v4().to_string(), &s, "x"));
    assert!(err.contains("ACCOUNT_REMOVED"), "{err}");
    assert!(home.join(".credentials.json").exists());
    let restored = f.api(&["account", "restore", "claude:gone"]);
    assert_eq!(restored["restored"], true);
    assert_eq!(f.api(&["account", "list"])[0]["selectable"], true);
    assert!(
        f.api_error(&["account", "restore", "claude:gone"])
            .contains("ACCOUNT_NOT_REMOVED")
    );

    // Deletion refuses while a verified live Claude session uses the home.
    let live = Command::new(f._tmp.path().join("fake-claude"))
        .args(["--session-id", &uuid::Uuid::new_v4().to_string()])
        .env("CLAUDE_CONFIG_DIR", &home)
        .current_dir(&f.project)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("live registry", || {
        f.claude_sessions()
            .iter()
            .any(|s| s["availability"] == "running")
    });
    let preview = f.api(&["account", "delete", "claude:gone"]);
    assert_eq!(preview["home_disposition"], "deleted");
    assert_eq!(preview["deletes_local_credentials"], true);
    let request = uuid::Uuid::new_v4().to_string();
    f.api(&[
        "account",
        "delete",
        "claude:gone",
        "--confirm",
        "claude:gone",
        "--request-id",
        &request,
    ]);
    let failed = f.deletion(&request);
    kill(live);
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
    assert!(home.join(".credentials.json").exists());
    assert_eq!(f.api(&["account", "list"]).as_array().unwrap().len(), 1);

    // Explicit recovery continues the original request.
    f.api(&["recover", &request]);
    let done = f.deletion(&request);
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["home_disposition"], "deleted");
    assert_eq!(done["history_preserved"], true);
    assert!(!home.exists(), "managed home and credentials removed");
    let vault = f
        .paths
        .root
        .join(sessionsation::layout::PRESERVED_CONVERSATIONS)
        .join(&request)
        .join("home");
    assert!(vault.join("history.jsonl").is_file());
    assert!(vault.join("todos").is_dir());
    assert!(
        std::fs::read_dir(vault.join("projects"))
            .unwrap()
            .flatten()
            .any(|d| d.path().join(format!("{native}.jsonl")).is_file())
    );
    assert!(!vault.join(".credentials.json").exists());
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

    // Adopted directory: only the record goes; the directory is left in place.
    let adopted = f._tmp.path().join("adopted-claude");
    std::fs::create_dir(&adopted).unwrap();
    std::fs::write(adopted.join(".credentials.json"), b"synthetic fixture").unwrap();
    std::fs::create_dir_all(adopted.join("projects/-x")).unwrap();
    std::fs::write(adopted.join("projects/-x/marker"), b"kept").unwrap();
    let out = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--config-dir",
        adopted.to_str().unwrap(),
        "ext",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let preview = f.api(&["account", "delete", "claude:ext"]);
    assert_eq!(preview["home_disposition"], "left_in_place");
    assert_eq!(preview["deletes_local_credentials"], false);
    let request = uuid::Uuid::new_v4().to_string();
    f.api(&[
        "account",
        "delete",
        "claude:ext",
        "--confirm",
        "claude:ext",
        "--request-id",
        &request,
    ]);
    let done = f.deletion(&request);
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["home_disposition"], "left_in_place");
    assert!(done["detail"].as_str().unwrap().contains("left in place"));
    assert_eq!(
        read(&adopted.join(".credentials.json")),
        "synthetic fixture"
    );
    assert_eq!(read(&adopted.join("projects/-x/marker")), "kept");
    assert!(
        !f.paths
            .root
            .join(sessionsation::layout::PRESERVED_CONVERSATIONS)
            .join(&request)
            .exists()
    );
    assert!(f.api(&["account", "list"]).as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_names_rename_any_provider_without_changing_identity() {
    let f = fixture().await;
    f.add_logged_in("personal");
    assert!(
        f.run(&["account", "add", "--platform", "codex", "work"])
            .status
            .success()
    );
    let row = |id: &str| {
        f.api(&["account", "list"])
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()
            .clone()
    };
    let codex_before = row("codex:work");
    let renamed = f.api(&[
        "account",
        "rename",
        "codex:work",
        "--display-name",
        "Office",
    ]);
    assert_eq!(renamed["display_name"], "Office");
    assert_eq!(renamed["account_id"], "codex:work");
    let out = f.run(&["account", "rename", "claude:personal", "Home-Max"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let codex = row("codex:work");
    assert_eq!(codex["display_name"], "Office");
    assert_eq!(codex["name"], "work");
    assert_eq!(codex["config_home"], codex_before["config_home"]);
    assert!(f.paths.root.join("accounts/codex/work").is_dir());
    assert_eq!(row("claude:personal")["display_name"], "Home-Max");
    assert!(f.home("personal").is_dir());
    // Display names collide with a name or display name of the same provider only.
    assert!(
        f.run(&["account", "add", "--platform", "codex", "second"])
            .status
            .success()
    );
    for display in ["Office", "work"] {
        let err = f.api_error(&[
            "account",
            "rename",
            "codex:second",
            "--display-name",
            display,
        ]);
        assert!(err.contains("ACCOUNT_DISPLAY_NAME_EXISTS"), "{err}");
    }
    assert!(
        f.api_error(&[
            "account",
            "rename",
            "codex:work",
            "--display-name",
            &"x".repeat(31)
        ])
        .contains("INVALID_DISPLAY_NAME")
    );
    assert!(
        f.api_error(&["account", "rename", "codex:work", "--display-name", "a b"])
            .contains("INVALID_DISPLAY_NAME")
    );
    let clash = f.run(&["account", "add", "--platform", "codex", "Office"]);
    assert!(String::from_utf8_lossy(&clash.stderr).contains("ACCOUNT_NAME_EXISTS"));
    // Human list shows `display (name)`; lookup by unambiguous display name.
    let table = String::from_utf8_lossy(&f.run(&["account", "list"]).stdout).into_owned();
    assert!(
        table.contains("Office (work)") && table.contains("Home-Max (personal)"),
        "{table}"
    );
    let out = f.run(&["account", "disable", "Office"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(row("codex:work")["enabled"], false);
    // Persisted through the supervisor (single writer), with an event.
    let db =
        rusqlite::Connection::open(f.paths.root.join(sessionsation::layout::REGISTRY_DB)).unwrap();
    let events: i64 = db
        .query_row(
            "SELECT count(*) FROM events WHERE kind='account.renamed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events, 2);
    let state = sessionsation::store::load_state_snapshot(&f.paths).unwrap();
    assert_eq!(
        state.accounts["work"].display_name.as_deref(),
        Some("Office")
    );
    // Reset with --clear (API) or an empty value.
    let cleared = f.api(&["account", "rename", "codex:work", "--clear"]);
    assert!(cleared["display_name"].is_null());
    let cleared = f.api(&["account", "rename", "claude:personal", "--display-name", ""]);
    assert!(cleared["display_name"].is_null());
    assert!(row("claude:personal")["display_name"].is_null());
    let caps = f.api(&["capabilities"]);
    for p in caps["providers"].as_array().unwrap() {
        assert_eq!(p["account_rename"], true);
    }
}

fn proc_start(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}
/// A transcript with metadata only (and a private message the API must never echo).
fn transcript(home: &Path, cwd: &Path, id: &str) -> PathBuf {
    let dir = home
        .join("projects")
        .join(cwd.to_string_lossy().replace('/', "-"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type":"user","sessionId":id,"cwd":cwd,"timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"private prompt"}})
        ),
    )
    .unwrap();
    path
}
fn append(path: &Path, record: Value) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{record}").unwrap();
}
fn registry(home: &Path, pid: u32, session: &str, cwd: &Path, kind: &str, job: Option<&str>) {
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let mut entry = json!({"pid":pid,"sessionId":session,"cwd":cwd,"procStart":proc_start(pid).to_string(),
        "kind":kind,"status":"idle","version":"2.1.312"});
    if let Some(job) = job {
        entry["jobId"] = json!(job);
    }
    std::fs::write(
        home.join("sessions").join(format!("{pid}.json")),
        entry.to_string(),
    )
    .unwrap();
}
fn by_conversation(f: &Fixture, id: &str) -> Value {
    f.claude_sessions()
        .into_iter()
        .find(|s| s["conversation_id"] == id)
        .unwrap_or_else(|| panic!("no session for {id}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_identity_follows_switches_continuations_and_killed_processes() {
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
    let other = uuid::Uuid::new_v4().to_string();
    transcript(&home, &f.project, &other);
    // A terminal-attached launch (tmux pane recorded before exec).
    let child = f
        .command(&["run", "--account", "claude:personal"])
        .current_dir(&f.project)
        .env("TMUX_PANE", "%42")
        .spawn()
        .unwrap();
    let pid = child.id();
    wait_for("fake claude launch", || {
        home.join("sessions").join(format!("{pid}.json")).exists()
    });
    let first = read(&home.join("fake-launch-new"))
        .lines()
        .find_map(|l| l.strip_prefix("session="))
        .unwrap()
        .to_owned();
    let a = by_conversation(&f, &first);
    assert_eq!(a["availability"], "running");
    assert_eq!(a["attachment"]["target"], "%42");
    assert_eq!(by_conversation(&f, &other)["availability"], "stopped");

    // The same process switches to the other conversation in its terminal (the
    // registry `sessionId` changes): the existing entry is re-attached and the
    // entry it left is released to stopped. Never two entries for one process.
    let registry_file = home.join("sessions").join(format!("{pid}.json"));
    let mut entry: Value = serde_json::from_str(&read(&registry_file)).unwrap();
    entry["sessionId"] = json!(other);
    std::fs::write(&registry_file, entry.to_string()).unwrap();
    let sessions = f.claude_sessions();
    assert_eq!(sessions.len(), 2, "no duplicate entry is created");
    let b = by_conversation(&f, &other);
    assert_eq!(b["availability"], "running");
    assert_eq!(b["pid"], pid);
    assert_eq!(b["attachment"]["target"], "%42");
    assert_eq!(b["attachment"]["pid"], pid);
    let a = by_conversation(&f, &first);
    assert_eq!(a["availability"], "stopped");
    assert!(a["attachment"].is_null(), "{a}");
    assert_eq!(a["capabilities"]["resume"], true);
    assert_eq!(
        f.claude_sessions()
            .iter()
            .filter(|s| s["attachment"]["pid"] == pid)
            .count(),
        1
    );

    // Outage: the process is killed. Its registry file and the catalog
    // attachment are left behind (plus an unreadable leftover of another
    // killed process); resume with the pre-outage generation still works.
    let generation = b["generation"].as_str().unwrap().to_owned();
    kill(child);
    let mut dead = Command::new("sleep").arg("30").spawn().unwrap();
    let dead_pid = dead.id();
    dead.kill().unwrap();
    dead.wait().unwrap();
    let leftover = home.join("sessions").join(format!("{dead_pid}.json"));
    std::fs::write(&leftover, b"{truncated").unwrap();
    std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o000)).unwrap();
    let id = b["id"].as_str().unwrap().to_owned();
    let resumed = f
        .command(&["resume", &id, "--expected-generation", &generation])
        .current_dir("/")
        .spawn()
        .unwrap();
    wait_for("resume after a killed process", || {
        read(&home.join("fake-launch-resume")).contains(&format!("session={other}"))
    });
    wait_for("resumed live", || {
        by_conversation(&f, &other)["availability"] == "running"
    });
    kill(resumed);
    let b = by_conversation(&f, &other);
    assert_eq!(b["availability"], "stopped");
    assert!(b["attachment"].is_null());
    assert_eq!(b["capabilities"]["resume"], true);

    // Background hand-off: Claude Code continues the conversation under a new
    // session id and appends a `continued-in` record to the old transcript.
    // Before the marker exists both are listed; afterwards the continuation
    // inherits the original entry (stable id, name, account) and the duplicate
    // is merged.
    let renamed = f.api(&[
        "rename",
        "--session",
        &id,
        "--expected-generation",
        b["generation"].as_str().unwrap(),
        "--name",
        "sessionstuffs",
    ]);
    assert_eq!(renamed["name"], "sessionstuffs");
    let next = uuid::Uuid::new_v4().to_string();
    transcript(&home, &f.project, &next);
    let duplicate = by_conversation(&f, &next);
    assert_ne!(duplicate["id"], id);
    let old_path = std::fs::read_dir(home.join("projects"))
        .unwrap()
        .flatten()
        .map(|d| d.path().join(format!("{other}.jsonl")))
        .find(|p| p.exists())
        .unwrap();
    append(
        &old_path,
        json!({"type":"continued-in","timestamp":"2026-10-28T00:00:00Z","sessionId":other,"continuedInSessionId":next}),
    );
    let sessions = f.claude_sessions();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    let lineage = by_conversation(&f, &next);
    assert_eq!(lineage["id"], id);
    assert_eq!(lineage["name"], "sessionstuffs");
    assert_eq!(lineage["account_id"], "claude:personal");
    assert_eq!(lineage["previous_conversation_ids"], json!([other]));
    assert!(!sessions.iter().any(|s| s["conversation_id"] == other));
    assert!(
        f.api_error(&[
            "session",
            "--session",
            duplicate["id"].as_str().unwrap(),
            "--expected-generation",
            duplicate["generation"].as_str().unwrap()
        ])
        .contains("SESSION_MERGED")
    );
    assert!(!f.api(&["state"]).to_string().contains("private prompt"));
    let caps = f.api(&["capabilities"]);
    assert_eq!(caps["contract_revision"], 32);
    assert_eq!(
        caps["session_identity"]["claude_continuation_marker"],
        "continued-in"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_to_herdr_attaches_background_sessions_only_and_never_duplicates() {
    let f = fixture().await;
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
    let ok = |args: &[&str]| {
        let out = hosted(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    };
    let err = |args: &[&str]| {
        let out = hosted(args);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let calls = || read(&state.join("calls"));
    assert!(
        f.run(&["account", "add", "--platform", "claude", "personal"])
            .status
            .success()
    );
    let home = f.home("personal");
    wait_for("auth status poll", || {
        f.api(&["account", "list"])[0]["auth_required"] == false
    });

    // A background-daemon session: a live process registered with kind "bg".
    let bg = uuid::Uuid::new_v4().to_string();
    transcript(&home, &f.project, &bg);
    let agent = Process(
        Command::new("sleep")
            .arg("600")
            .env("CLAUDE_CONFIG_DIR", &home)
            .spawn()
            .unwrap(),
    );
    let agent_pid = agent.0.id();
    wait_for("agent exec", || {
        read(Path::new(&format!("/proc/{agent_pid}/cmdline"))).starts_with("sleep")
    });
    registry(&home, agent_pid, &bg, &f.project, "bg", Some(&bg[..8]));
    // An interactive session in another terminal.
    let interactive = f
        .command(&["run", "--account", "claude:personal"])
        .current_dir(&f.project)
        .spawn()
        .unwrap();
    wait_for("interactive launch", || {
        home.join("fake-launch-new").exists()
    });
    let other = read(&home.join("fake-launch-new"))
        .lines()
        .find_map(|l| l.strip_prefix("session="))
        .unwrap()
        .to_owned();
    wait_for("interactive live", || {
        ok(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["conversation_id"] == other && s["availability"] == "running")
    });
    let find = |id: &str| -> Value {
        ok(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["conversation_id"] == id)
            .unwrap()
            .clone()
    };
    let s = find(&bg);
    assert_eq!(s["availability"], "running");
    assert_eq!(s["native"]["kind"], "bg");
    assert_eq!(s["capabilities"]["add_to_herdr"], true, "{s}");
    let i = find(&other);
    assert_eq!(i["capabilities"]["add_to_herdr"], false);
    assert!(
        i["capability_reasons"]["add_to_herdr"]
            .as_str()
            .unwrap()
            .starts_with("INTERACTIVE_ELSEWHERE")
    );
    let e = err(&[
        "session",
        "add-to-herdr",
        "--session",
        i["id"].as_str().unwrap(),
        "--expected-generation",
        i["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
    ]);
    assert!(e.contains("INTERACTIVE_ELSEWHERE"), "{e}");
    assert!(!calls().contains("tab create"), "refusal creates nothing");

    // Spaces of the chosen HERDR server for a session not in HERDR yet.
    let spaces = ok(&[
        "spaces",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
    ]);
    assert_eq!(spaces["spaces"][0]["id"], "w1");
    assert!(spaces["current_space_id"].is_null());

    let added = ok(&[
        "session",
        "add-to-herdr",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
        "--name",
        "bg-view",
    ]);
    assert_eq!(added["state"], "attached", "{added}");
    assert_eq!(added["target"], "herdr/fixture/w1:p2");
    assert_eq!(added["starts_agent"], false);
    assert_eq!(read(&state.join("label-w1:p2")).trim(), "bg-view");
    let attach = read(&home.join("fake-attach"));
    assert!(attach.contains(&format!("attach={}", &bg[..8])), "{attach}");
    assert!(attach.contains(&format!("config_dir={}", home.display())));
    let client: u32 = read(&state.join("pid-w1:p2")).trim().parse().unwrap();
    assert_eq!(added["attachment"]["pid"], client);
    let after = find(&bg);
    assert_eq!(after["id"], s["id"], "same catalog entry");
    assert_eq!(after["name"], "bg-view");
    assert_eq!(after["pid"], agent_pid, "the agent process is unchanged");
    assert_eq!(after["attachment"]["host"], "herdr");
    assert_eq!(after["attachment"]["pid"], client);
    assert_eq!(after["capabilities"]["add_to_herdr"], false);
    assert!(
        after["capability_reasons"]["add_to_herdr"]
            .as_str()
            .unwrap()
            .starts_with("ALREADY_IN_HERDR")
    );
    // Never a second client or tab for a live session.
    let e = err(&[
        "session",
        "add-to-herdr",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        after["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
    ]);
    assert!(e.contains("ALREADY_IN_HERDR"), "{e}");
    assert_eq!(calls().matches("tab create").count(), 1);
    assert_eq!(
        ok(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["conversation_id"] == bg)
            .count(),
        1
    );

    // New Space: refused for a label that exists or alongside --space, creates
    // nothing; with a fresh label the new Space's own shell pane hosts the viewer.
    unsafe {
        libc::kill(client as i32, libc::SIGKILL);
    }
    wait_for("viewer gone", || {
        find(&bg)["capabilities"]["add_to_herdr"] == true
    });
    let again = find(&bg);
    let base = |extra: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = [
            "session",
            "add-to-herdr",
            "--session",
            again["id"].as_str().unwrap(),
            "--expected-generation",
            again["generation"].as_str().unwrap(),
            "--herdr-session",
            "fixture",
        ]
        .map(String::from)
        .into();
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    let run = |extra: &[&str]| {
        let v = base(extra);
        let refs: Vec<&str> = v.iter().map(String::as_str).collect();
        hosted(&refs)
    };
    let exists = run(&["--new-space", "Main"]);
    assert!(String::from_utf8_lossy(&exists.stderr).contains("WORKSPACE_NAME_EXISTS"));
    assert!(!run(&["--new-space", "X", "--space", "w1"]).status.success());
    assert!(!run(&[]).status.success());
    assert!(!run(&["--new-space", "bad/label"]).status.success());
    assert!(!run(&["--new-space", " Padded"]).status.success());
    assert!(
        !calls().contains("workspace create"),
        "refusals create nothing"
    );
    let fresh = run(&["--new-space", "Fresh Space", "--name", "new-view"]);
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    let fresh: Value = serde_json::from_slice(&fresh.stdout).unwrap();
    let fresh = &fresh["result"];
    assert_eq!(fresh["state"], "attached", "{fresh}");
    assert_eq!(fresh["space_id"], "w9");
    assert_eq!(fresh["target"], "herdr/fixture/w9:p1");
    assert_eq!(calls().matches("workspace create").count(), 1);
    assert!(calls().contains("--label Fresh Space"), "{}", calls());
    assert_eq!(
        calls().matches("tab create").count(),
        1,
        "no extra tab or pane"
    );
    assert!(!calls().contains("pane close"));
    assert_eq!(read(&state.join("renamed-w9:t1")).trim(), "new-view");

    // Codex: not supported, with the reason.
    let codex =
        sessionsation::catalog::record_launch(&f.paths, "personal", "thread", "codex", None)
            .unwrap();
    let e = err(&[
        "session",
        "add-to-herdr",
        "--session",
        &codex.id,
        "--expected-generation",
        &codex.generation(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
    ]);
    assert!(e.contains("UNSUPPORTED_PROVIDER_OPERATION"), "{e}");

    // Start Session of a stopped session into an existing Space with a name.
    kill(interactive);
    let stopped = find(&other);
    assert_eq!(stopped["availability"], "stopped");
    let creates = calls().matches("workspace create").count();
    for extra in [
        vec!["--new-space", "Main"],
        vec!["--new-space", "Z", "--space", "w1"],
    ] {
        let mut v = vec![
            "resume",
            "--session",
            stopped["id"].as_str().unwrap(),
            "--expected-generation",
            stopped["generation"].as_str().unwrap(),
            "--herdr-session",
            "fixture",
        ];
        v.extend(extra);
        let message = err(&v);
        assert!(
            message.contains("WORKSPACE_NAME_EXISTS") || message.contains("cannot be used"),
            "{message}"
        );
    }
    assert_eq!(calls().matches("workspace create").count(), creates);
    let resumed = ok(&[
        "resume",
        "--session",
        stopped["id"].as_str().unwrap(),
        "--expected-generation",
        stopped["generation"].as_str().unwrap(),
        "--herdr-session",
        "fixture",
        "--space",
        "w1",
        "--name",
        "resumed-tab",
    ]);
    assert_eq!(resumed["state"], "running", "{resumed}");
    assert_eq!(resumed["target"], "herdr/fixture/w1:p3");
    assert_eq!(read(&state.join("label-w1:p3")).trim(), "resumed-tab");
    let live = find(&other);
    assert_eq!(live["name"], "resumed-tab");
    assert_eq!(live["availability"], "running");
    for pane in ["w1:p2", "w1:p3", "w9:p1"] {
        if let Ok(pid) = read(&state.join(format!("pid-{pane}")))
            .trim()
            .parse::<i32>()
        {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    drop(agent);
}

/// Live regression: two continued-in chains, the newest conversation of one is a
/// running background session, and a pre-revision-12 release keeps rewriting the
/// catalog without `aliases`/`previous_conversation_ids` and re-registering the
/// old conversations. Identity must never be deleted and recreated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuation_identity_is_stable_across_repeated_state_calls_and_old_writers() {
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
    let [a1, a2, b1, b2] = std::array::from_fn(|_| uuid::Uuid::new_v4().to_string());
    let paths: Vec<_> = [&a1, &a2, &b1, &b2]
        .iter()
        .map(|id| transcript(&home, &f.project, id))
        .collect();
    // a2 is live in the background daemon (registry sessionId = newest id).
    let agent = Process(
        Command::new("sleep")
            .arg("600")
            .env("CLAUDE_CONFIG_DIR", &home)
            .spawn()
            .unwrap(),
    );
    let pid = agent.0.id();
    wait_for("agent exec", || {
        read(Path::new(&format!("/proc/{pid}/cmdline"))).starts_with("sleep")
    });
    registry(&home, pid, &a2, &f.project, "bg", Some(&a2[..8]));
    // Before the hand-offs every conversation has its own entry.
    let before = f.claude_sessions();
    assert_eq!(before.len(), 4, "{before:?}");
    let entry = |c: &str| by_conversation(&f, c);
    let (ea1, ea2, eb1, eb2) = (entry(&a1), entry(&a2), entry(&b1), entry(&b2));
    let ea1 = f.api(&[
        "rename",
        "--session",
        ea1["id"].as_str().unwrap(),
        "--expected-generation",
        ea1["generation"].as_str().unwrap(),
        "--name",
        "sessionstuffs-claude",
    ]);
    for (old, new) in [(&paths[0], &a2), (&paths[2], &b2)] {
        let from = old.file_stem().unwrap().to_str().unwrap();
        append(
            old,
            json!({"type":"continued-in","timestamp":"2026-10-28T00:00:00Z","sessionId":from,"continuedInSessionId":new}),
        );
    }
    let snapshot = || {
        let mut v: Vec<_> = f
            .claude_sessions()
            .into_iter()
            .map(|s| {
                (
                    s["id"].as_str().unwrap().to_owned(),
                    s["name"].as_str().unwrap().to_owned(),
                    s["conversation_id"].as_str().unwrap().to_owned(),
                    s["previous_conversation_ids"].clone(),
                    s["availability"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        v.sort_by(|x, y| x.0.cmp(&y.0));
        v
    };
    let expected = snapshot();
    assert_eq!(expected.len(), 2, "{expected:?}");
    let a = expected.iter().find(|s| s.0 == ea1["id"]).unwrap();
    assert_eq!(
        (a.1.as_str(), a.2.as_str(), &a.3, a.4.as_str()),
        ("sessionstuffs-claude", a2.as_str(), &json!([a1]), "running")
    );
    let b = expected.iter().find(|s| s.0 == eb1["id"]).unwrap();
    assert_eq!(
        (&b.1, b.2.as_str(), &b.3),
        (
            &eb1["name"].as_str().unwrap().to_owned(),
            b2.as_str(),
            &json!([b1])
        )
    );
    for _ in 0..5 {
        assert_eq!(snapshot(), expected);
    }
    // Merged ids resolve to SESSION_MERGED for lookups and actions.
    let merged_error = |dup: &Value, survivor: &Value| {
        let id = dup["id"].as_str().unwrap();
        let generation = dup["generation"].as_str().unwrap();
        for args in [
            vec![
                "session",
                "--session",
                id,
                "--expected-generation",
                generation,
            ],
            vec![
                "rename",
                "--session",
                id,
                "--expected-generation",
                generation,
                "--name",
                "x",
            ],
        ] {
            let e = f.api_error(&args);
            assert!(
                e.contains("SESSION_MERGED") && e.contains(survivor["id"].as_str().unwrap()),
                "{e}"
            );
        }
    };
    merged_error(&ea2, &ea1);
    merged_error(&eb2, &eb1);

    // An older release rewrites the catalog: drops both identity fields and
    // registers the continued conversations again. The next calls restore the
    // exact identity (never steal it) and keep every id resolvable.
    let file = f
        .paths
        .root
        .join(sessionsation::layout::SESSION_CATALOG)
        .join("catalog.json");
    let mut stray = vec![];
    for round in 0..2 {
        let mut v: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("aliases");
        for s in v["sessions"].as_object_mut().unwrap().values_mut() {
            s.as_object_mut()
                .unwrap()
                .remove("previous_conversation_ids");
        }
        for c in [&a1, &b1] {
            let id = uuid::Uuid::new_v4().to_string();
            v["sessions"][&id] = json!({"id":id,"name":format!("projects-{round}{}", &c[..4]),"provider":"claude",
                "account":"personal","conversation_id":c,"revision":1,"attachment":null,"directory":f.project});
            stray.push(id);
        }
        std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
        for _ in 0..5 {
            assert_eq!(snapshot(), expected);
        }
    }
    let repaired = f.api(&["session", "repair-identity"]);
    assert_eq!(repaired["restored_aliases"], json!([]));
    assert_eq!(repaired["merged"], json!([]));
    assert_eq!(repaired["lineages"], 2);
    assert_eq!(repaired["unresolved_ids"], json!([]));
    assert_eq!(snapshot(), expected);
    for id in &stray {
        let e = f.api_error(&["session", "--session", id, "--expected-generation", "x"]);
        assert!(e.contains("SESSION_MERGED"), "{e}");
    }
    merged_error(&ea2, &ea1);
    drop(agent);
}

fn logout_events(f: &Fixture) -> Vec<String> {
    let db =
        rusqlite::Connection::open(f.paths.root.join(sessionsation::layout::REGISTRY_DB)).unwrap();
    db.prepare("SELECT entity FROM events WHERE kind='account.logged_out' ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_logout_uses_the_official_command_refuses_in_use_and_is_idempotent() {
    let f = fixture().await;
    assert!(
        f.run(&[
            "account",
            "add",
            "--platform",
            "claude",
            "work",
            "--no-login"
        ])
        .status
        .success()
    );
    let home = f.home("work");
    f.api(&["account", "refresh", "claude:work"]);
    assert_eq!(f.api(&["account", "list"])[0]["selectable"], true);

    // A live provider process using the account refuses the logout.
    let agent = Process(
        Command::new("sleep")
            .arg("600")
            .env("CLAUDE_CONFIG_DIR", &home)
            .spawn()
            .unwrap(),
    );
    let pid = agent.0.id();
    wait_for("agent exec", || {
        read(Path::new(&format!("/proc/{pid}/cmdline"))).starts_with("sleep")
    });
    let live = uuid::Uuid::new_v4().to_string();
    registry(&home, pid, &live, &f.project, "bg", Some(&live[..8]));
    let refused = f.api_error(&["account", "logout", "claude:work"]);
    assert!(refused.contains("ACCOUNT_IN_USE"), "{refused}");
    assert!(!home.join("fake-logout").exists());
    assert_eq!(
        f.api(&["account", "refresh", "claude:work"])["auth_required"],
        false
    );
    drop(agent);

    let out = f.api(&["account", "logout", "claude:work"]);
    assert_eq!(
        out,
        json!({"account_id":"claude:work","provider":"claude","logged_out":true,"auth_required":true,
            "method":"claude auth logout"})
    );
    let record = read(&home.join("fake-logout"));
    assert!(
        record.contains(&format!("config_dir={}", home.display())),
        "{record}"
    );
    assert!(record.contains("args=auth logout"), "{record}");
    let row = f.api(&["account", "list"])[0].clone();
    assert_eq!(row["auth_required"], true);
    assert_eq!(row["selectable"], false);
    assert_eq!(row["auth_detail"], "AUTH_REQUIRED");
    assert_eq!(
        f.api(&["account", "refresh", "claude:work"])["auth_required"],
        true,
        "the supervisor's own probe agrees"
    );
    assert_eq!(
        f.api(&["account", "login", "claude:work"])["login"]["action"],
        "exec"
    );
    let capabilities = f.api(&["capabilities"]);
    assert!(capabilities["contract_revision"].as_i64().unwrap() >= 14);
    for provider in capabilities["providers"].as_array().unwrap() {
        // Antigravity signs out only inside agy (/logout): declared unsupported.
        let expected = provider["id"] != "antigravity";
        assert_eq!(provider["account_logout"], expected);
        assert_eq!(
            provider["operations"]["account_logout"]["supported"],
            expected
        );
        assert!(provider["operations"]["account_logout"]["reason"].is_string());
    }
    assert_eq!(logout_events(&f), vec!["work".to_owned()]);

    // Already logged out: the same call succeeds without a second event.
    let again = f.api(&["account", "logout", "claude:work"]);
    assert_eq!(again["logged_out"], true);
    assert_eq!(logout_events(&f).len(), 1);
    let human = f.run(&["account", "logout", "work"]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("claude:work logged out"));

    // Unknown, mismatched and de-registered accounts get clear errors.
    assert!(
        f.api_error(&["account", "logout", "claude:ghost"])
            .contains("ACCOUNT_NOT_FOUND")
    );
    assert!(
        f.api_error(&["account", "logout", "codex:work"])
            .contains("ACCOUNT_PROVIDER_MISMATCH")
    );
    let marker = f
        .paths
        .root
        .join(sessionsation::layout::ACCOUNT_CATALOG)
        .join("claude");
    std::fs::create_dir_all(&marker).unwrap();
    std::fs::write(marker.join("work.removed.json"), "{}").unwrap();
    assert!(
        f.api_error(&["account", "logout", "claude:work"])
            .contains("ACCOUNT_REMOVED")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_logout_goes_through_the_account_helper_and_refuses_a_held_lease() {
    use fs2::FileExt;
    let f = fixture().await;
    assert!(
        f.run(&["account", "add", "--platform", "codex", "cx"])
            .status
            .success()
    );
    let home = f.paths.account("cx").unwrap();
    wait_for("codex auth observed", || {
        f.api(&["account", "list"])
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "codex:cx" && a["auth_required"] == false)
    });
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(".switcher-lease"))
        .unwrap();
    lease.try_lock_exclusive().unwrap();
    let refused = f.api_error(&["account", "logout", "codex:cx"]);
    assert!(refused.contains("ACCOUNT_IN_USE"), "{refused}");
    assert!(!home.join("fake-logout").exists());
    FileExt::unlock(&lease).unwrap();

    let out = f.api(&["account", "logout", "codex:cx"]);
    assert_eq!(out["account_id"], "codex:cx");
    assert_eq!(out["provider"], "codex");
    assert_eq!(out["logged_out"], true);
    assert_eq!(out["auth_required"], true);
    assert!(out["method"].as_str().unwrap().contains("account/logout"));
    assert_eq!(read(&home.join("fake-logout")).trim(), "rpc");
    assert!(!home.join("auth.json").exists());
    let row = f.api(&["account", "list"])[0].clone();
    assert_eq!(row["auth_required"], true);
    assert_eq!(row["selectable"], false);
    // The supervisor's poll keeps agreeing and never revives the login.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(f.api(&["account", "list"])[0]["auth_required"], true);
    let again = f.api(&["account", "logout", "codex:cx"]);
    assert_eq!(again["logged_out"], true);
    assert_eq!(logout_events(&f), vec!["cx".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_order_is_persistent_shared_and_reorderable() {
    let f = fixture().await;
    let ids = |f: &Fixture| -> Vec<String> {
        f.api(&["account", "list"])
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(
        f.run(&["account", "add", "--platform", "codex", "zeta"])
            .status
            .success()
    );
    assert!(
        f.run(&[
            "account",
            "add",
            "--platform",
            "claude",
            "alpha",
            "--no-login"
        ])
        .status
        .success()
    );
    assert!(
        f.run(&["account", "add", "--platform", "codex", "mid"])
            .status
            .success()
    );
    // New accounts are appended in enrollment order, not alphabetically.
    assert_eq!(ids(&f), ["codex:zeta", "claude:alpha", "codex:mid"]);
    let moved = f.api(&["account", "reorder", "codex:mid", "--position", "0"]);
    assert_eq!(moved["changed"], true);
    assert_eq!(
        moved["order"],
        json!(["codex:mid", "codex:zeta", "claude:alpha"])
    );
    assert_eq!(ids(&f), ["codex:mid", "codex:zeta", "claude:alpha"]);
    let state_ids: Vec<String> = f.api(&["state"])["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(state_ids, ids(&f));
    // The human command moves too, and an unchanged position is a no-op.
    assert!(
        f.run(&["account", "reorder", "alpha", "--position", "2"])
            .status
            .success()
    );
    assert_eq!(ids(&f), ["codex:mid", "codex:zeta", "claude:alpha"]);
    assert_eq!(
        f.api(&["account", "reorder", "claude:alpha", "--position", "2"])["changed"],
        false
    );
    assert!(
        f.api_error(&["account", "reorder", "claude:alpha", "--position", "3"])
            .contains("INVALID_POSITION")
    );
    assert!(
        f.api_error(&["account", "reorder", "claude:ghost", "--position", "0"])
            .contains("ACCOUNT_NOT_FOUND")
    );
    let caps = f.api(&["capabilities"]);
    assert_eq!(caps["account_reorder"], true);
    assert_eq!(caps["state_cached"], true);
    let events = {
        let db = rusqlite::Connection::open(f.paths.root.join(sessionsation::layout::REGISTRY_DB))
            .unwrap();
        db.prepare("SELECT entity FROM events WHERE kind='account.reordered' ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
    };
    assert_eq!(events, ["mid"]);
}

// ------------------------------------------------- archive, restore, delete (revision 17)

/// Admit a conversation job and wait for its terminal phase.
fn conversation_job(f: &Fixture, command: &str, session: &Value) -> Value {
    let request = uuid::Uuid::new_v4().to_string();
    let id = session["id"].as_str().unwrap();
    let op = f.api(&[
        command,
        "--session",
        id,
        "--expected-generation",
        session["generation"].as_str().unwrap(),
        "--request-id",
        &request,
        "--confirm",
        id,
    ]);
    assert_eq!(op["id"], request.as_str());
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
    last
}
fn all_claude(f: &Fixture) -> Vec<Value> {
    f.api(&["state", "--archive-filter", "all"])["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["provider"] == "claude")
        .cloned()
        .collect()
}
fn job_error(f: &Fixture, command: &str, session: &Value) -> String {
    let id = session["id"].as_str().unwrap();
    f.api_error(&[
        command,
        "--session",
        id,
        "--expected-generation",
        session["generation"].as_str().unwrap(),
        "--request-id",
        &uuid::Uuid::new_v4().to_string(),
        "--confirm",
        id,
    ])
}
fn tree(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = vec![];
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push((p.strip_prefix(base).unwrap().to_path_buf(), read(&p)));
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_sessions_archive_restore_and_delete_only_their_own_files() {
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
    let (mine, other) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    let path = transcript(&home, &f.project, &mine);
    transcript(&home, &f.project, &other);
    let dir = path.parent().unwrap().to_path_buf();
    for (p, body) in [
        (dir.join(&mine).join("subagents/agent-a1.jsonl"), "sub"),
        (dir.join(&mine).join("tool-results/t1.txt"), "tool"),
        (
            dir.join(&other).join("subagents/agent-b1.jsonl"),
            "other sub",
        ),
        (home.join("file-history").join(&mine).join("v1"), "fh"),
        (
            home.join("file-history").join(&other).join("v1"),
            "other fh",
        ),
        (home.join(format!("todos/{mine}-agent-{mine}.json")), "[]"),
        (home.join(format!("todos/{other}-agent-{other}.json")), "[]"),
        (home.join(".credentials.json"), "synthetic credential"),
    ] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let before = tree(&home);
    let s = by_conversation(&f, &mine);
    assert_eq!(s["capabilities"]["archive"], true, "{s}");
    assert_eq!(s["capabilities"]["delete"], true);
    assert_eq!(s["capabilities"]["restore"], false);
    let caps = f.api(&["capabilities"]);
    assert_eq!(caps["contract_revision"], 32);
    assert_eq!(caps["conversation_archive"]["native"]["since_revision"], 17);
    let claude = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "claude")
        .unwrap()
        .clone();
    assert_eq!(claude["operations"]["archive"]["supported"], true);
    assert_eq!(claude["operations"]["delete_session"]["supported"], true);
    // Confirmation must name the exact session.
    let wrong = f.api_error(&[
        "archive",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--request-id",
        &uuid::Uuid::new_v4().to_string(),
        "--confirm",
        &mine,
    ]);
    assert!(wrong.contains("CONFIRMATION_REQUIRED"), "{wrong}");

    // Archive moves exactly this conversation's files out of the account home.
    conversation_job(&f, "archive", &s);
    for p in [
        path.clone(),
        dir.join(&mine),
        home.join("file-history").join(&mine),
        home.join(format!("todos/{mine}-agent-{mine}.json")),
    ] {
        assert!(!p.exists(), "{} still in the account home", p.display());
    }
    let kept: Vec<_> = tree(&home);
    assert!(
        before
            .iter()
            .filter(|(p, _)| !p.to_string_lossy().contains(&mine))
            .all(|e| kept.contains(e)),
        "other sessions' files and credentials are unchanged"
    );
    assert!(
        f.paths
            .root
            .join(sessionsation::layout::ARCHIVED_SESSIONS)
            .join(s["id"].as_str().unwrap())
            .join("native.json")
            .is_file()
    );
    assert!(
        !f.claude_sessions().iter().any(|x| x["id"] == s["id"]),
        "hidden from the default (active) state"
    );
    let archived = all_claude(&f)
        .into_iter()
        .find(|x| x["id"] == s["id"])
        .unwrap();
    assert_eq!(archived["availability"], "archived");
    assert_eq!(archived["archived"], true);
    assert_eq!(archived["archive"]["kind"], "managed");
    assert_eq!(archived["capabilities"]["restore"], true);
    assert_eq!(archived["capabilities"]["resume"], false);
    assert_eq!(archived["capabilities"]["archive"], false);
    assert_eq!(archived["resume_block_reason"], "archived");
    let listed = f.api(&["archives"]);
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == s["id"])
        .unwrap();
    assert_eq!(row["provider"], "claude");
    assert_eq!(row["account_id"], "claude:personal");
    assert_eq!(row["generation"], archived["generation"]);
    assert!(
        job_error(&f, "archive", &archived).contains("SESSION_ALREADY_ARCHIVED"),
        "archive is not repeated"
    );
    assert!(
        f.api_error(&[
            "launcher",
            "resume",
            "--session",
            s["id"].as_str().unwrap(),
            "--expected-generation",
            archived["generation"].as_str().unwrap()
        ])
        .contains("SESSION_STALE"),
        "an archived transcript cannot be resumed"
    );

    // Restore returns the identical files to their original paths.
    conversation_job(&f, "restore-session", &archived);
    assert_eq!(tree(&home), before);
    let restored = by_conversation(&f, &mine);
    assert_eq!(restored["id"], s["id"]);
    assert_eq!(restored["availability"], "stopped");
    assert_eq!(restored["capabilities"]["resume"], true);
    assert_eq!(restored["capabilities"]["archive"], true);
    assert!(
        job_error(&f, "restore-session", &restored).contains("SESSION_NOT_ARCHIVED"),
        "restore needs an archived session"
    );

    // Delete removes the files, dead registry entries and the catalog entry.
    let dead = Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    let mut dead = dead;
    dead.wait().unwrap();
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(
        home.join(format!("sessions/{dead_pid}.json")),
        json!({"pid":dead_pid,"sessionId":mine,"procStart":"1"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        home.join("sessions/1.json"),
        json!({"pid":1,"sessionId":other,"procStart":"1"}).to_string(),
    )
    .unwrap();
    let op = conversation_job(&f, "delete-session", &by_conversation(&f, &mine));
    assert!(!path.exists() && !dir.join(&mine).exists());
    assert!(!home.join("file-history").join(&mine).exists());
    assert!(!home.join(format!("sessions/{dead_pid}.json")).exists());
    assert!(
        home.join("sessions/1.json").exists(),
        "another session's entry"
    );
    assert!(
        !f.paths
            .root
            .join(sessionsation::layout::DELETIONS)
            .join(format!("native-{}", op["id"].as_str().unwrap()))
            .exists(),
        "staging is removed after completion"
    );
    assert_eq!(
        read(&home.join(".credentials.json")),
        "synthetic credential"
    );
    assert!(dir.join(format!("{other}.jsonl")).exists());
    assert_eq!(
        read(&dir.join(&other).join("subagents/agent-b1.jsonl")),
        "other sub"
    );
    assert!(!all_claude(&f).iter().any(|x| x["id"] == s["id"]));
    let gone = f.api_error(&[
        "session",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        restored["generation"].as_str().unwrap(),
    ]);
    assert!(gone.contains("SESSION_NOT_FOUND"), "{gone}");
    // Never re-registered from leftovers.
    assert!(
        !all_claude(&f)
            .iter()
            .any(|x| x["conversation_id"] == mine.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_sessions_are_refused_and_merged_lineages_move_together() {
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
    let (a1, a2) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    let p1 = transcript(&home, &f.project, &a1);
    let p2 = transcript(&home, &f.project, &a2);
    let dir = p1.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(dir.join(&a1).join("subagents")).unwrap();
    std::fs::write(dir.join(&a1).join("subagents/agent-x.jsonl"), "x").unwrap();
    let (e1, e2) = (by_conversation(&f, &a1), by_conversation(&f, &a2));
    append(
        &p1,
        json!({"type":"continued-in","timestamp":"2026-10-28T00:00:00Z","sessionId":a1,"continuedInSessionId":a2}),
    );
    let survivor = by_conversation(&f, &a2);
    assert_eq!(survivor["id"], e1["id"]);
    assert_eq!(survivor["previous_conversation_ids"], json!([a1]));

    // Live: a verified registry entry names the newest conversation.
    let agent = Command::new("sleep")
        .arg("600")
        .env("CLAUDE_CONFIG_DIR", &home)
        .spawn()
        .unwrap();
    let pid = agent.id();
    wait_for("agent exec", || {
        read(Path::new(&format!("/proc/{pid}/cmdline"))).starts_with("sleep")
    });
    registry(&home, pid, &a2, &f.project, "interactive", None);
    let live = by_conversation(&f, &a2);
    assert_eq!(live["availability"], "running");
    for k in ["archive", "delete"] {
        assert_eq!(live["capabilities"][k], false);
        assert!(
            live["capability_reasons"][k]
                .as_str()
                .unwrap()
                .starts_with("SESSION_RUNNING"),
            "{live}"
        );
    }
    for command in ["archive", "delete-session"] {
        let e = job_error(&f, command, &live);
        assert!(e.contains("SESSION_RUNNING"), "{e}");
    }
    assert!(p1.exists() && p2.exists(), "nothing moved while running");
    kill(agent);

    // The merged id is refused with its survivor; the survivor moves its whole lineage.
    let stopped = by_conversation(&f, &a2);
    assert_eq!(stopped["availability"], "stopped");
    let merged = job_error(&f, "archive", &e2);
    assert!(
        merged.contains("SESSION_MERGED") && merged.contains(e1["id"].as_str().unwrap()),
        "{merged}"
    );
    conversation_job(&f, "archive", &stopped);
    assert!(!p1.exists() && !p2.exists() && !dir.join(&a1).exists());
    assert_eq!(
        all_claude(&f)
            .iter()
            .filter(|x| x["id"] == e1["id"] || x["id"] == e2["id"])
            .count(),
        1,
        "the old conversation is not re-registered while archived"
    );
    let archived = all_claude(&f)
        .into_iter()
        .find(|x| x["id"] == e1["id"])
        .unwrap();
    conversation_job(&f, "restore-session", &archived);
    assert!(p1.exists() && p2.exists());
    assert_eq!(read(&dir.join(&a1).join("subagents/agent-x.jsonl")), "x");
    let back = by_conversation(&f, &a2);
    assert_eq!(back["id"], e1["id"]);
    assert_eq!(back["previous_conversation_ids"], json!([a1]));

    conversation_job(&f, "delete-session", &back);
    assert!(!p1.exists() && !p2.exists() && !dir.join(&a1).exists());
    assert!(all_claude(&f).is_empty());
    // The merged alias goes with its survivor, also from the identity record.
    let identity = read(
        &f.paths
            .root
            .join(sessionsation::layout::SESSION_CATALOG)
            .join("identity.json"),
    );
    assert!(!identity.contains(e2["id"].as_str().unwrap()), "{identity}");
    let e = f.api_error(&[
        "session",
        "--session",
        e2["id"].as_str().unwrap(),
        "--expected-generation",
        e2["generation"].as_str().unwrap(),
    ]);
    assert!(e.contains("SESSION_NOT_FOUND"), "{e}");
}

fn set_registry_field(home: &Path, pid: u32, key: &str, value: Value) {
    let file = home.join("sessions").join(format!("{pid}.json"));
    let mut entry: Value = serde_json::from_str(&read(&file)).unwrap();
    if value.is_null() {
        entry.as_object_mut().unwrap().remove(key);
    } else {
        entry[key] = value;
    }
    std::fs::write(file, entry.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_since_comes_from_the_registry_never_a_bg_spare_and_is_null_when_unknown() {
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
    let spawn = |args: &[&str]| {
        let p = Process(
            Command::new(args[0])
                .args(&args[1..])
                .env("CLAUDE_CONFIG_DIR", &home)
                .spawn()
                .unwrap(),
        );
        let pid = p.0.id();
        wait_for("process exec", || {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| c.starts_with(args[0].as_bytes()))
        });
        p
    };
    let id = || uuid::Uuid::new_v4().to_string();
    let (live, no_start, bogus, spare, stopped) = (id(), id(), id(), id(), id());
    for c in [&live, &no_start, &bogus, &spare, &stopped] {
        transcript(&home, &f.project, c);
    }
    let started = now() - 3600;
    // Registry startedAt (milliseconds) after the process start and not in the
    // future is believed; the process is made two seconds older first.
    let a = spawn(&["sleep", "600"]);
    std::thread::sleep(Duration::from_millis(2100));
    let registry_second = now();
    registry(&home, a.0.id(), &live, &f.project, "interactive", None);
    set_registry_field(
        &home,
        a.0.id(),
        "startedAt",
        json!(registry_second * 1000 + 250),
    );
    // No startedAt: the verified /proc process start is the honest source.
    let b = spawn(&["sleep", "601"]);
    registry(&home, b.0.id(), &no_start, &f.project, "interactive", None);
    // A startedAt long before the process even existed is not believed.
    let c = spawn(&["sleep", "602"]);
    registry(&home, c.0.id(), &bogus, &f.project, "interactive", None);
    set_registry_field(&home, c.0.id(), "startedAt", json!(started * 1000));
    // A pre-started bg-spare: its own start and startedAt are not the session's.
    let d = spawn(&["sh", "-c", "sleep 603; :", "x", "--bg-spare"]);
    registry(&home, d.0.id(), &spare, &f.project, "bg", Some(&spare[..8]));
    set_registry_field(&home, d.0.id(), "startedAt", json!(started * 1000));

    let at = |conversation: &str| by_conversation(&f, conversation)["active_since"].clone();
    let n = now();
    assert_eq!(at(&live), registry_second, "registry startedAt");
    for process_only in [&no_start, &bogus] {
        let v = at(process_only).as_i64().expect("process start");
        assert!((n - 30..=n + 1).contains(&v), "{v} vs {n}");
        assert_ne!(v, started);
    }
    let row = by_conversation(&f, &spare);
    assert_eq!(row["availability"], "running", "{row}");
    assert!(
        row["active_since"].is_null(),
        "a bg-spare start is never the session start: {row}"
    );
    let row = by_conversation(&f, &stopped);
    assert_eq!(row["availability"], "stopped");
    assert!(row["active_since"].is_null(), "{row}");
}

/// Audit C: deleting a throwaway session removes its now-empty project dir and
/// redacts the deleted manifest to ids; other dirs and non-empty ones stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_removes_only_its_own_empty_project_dir_and_redacts_the_manifest() {
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
    let mine = uuid::Uuid::new_v4().to_string();
    let path = transcript(&home, &f.project, &mine);
    let dir = path.parent().unwrap().to_path_buf();
    let unrelated = home.join("projects/-tmp-someone-elses-empty");
    std::fs::create_dir_all(&unrelated).unwrap();
    let session = by_conversation(&f, &mine);
    conversation_job(&f, "delete-session", &session);
    assert!(!dir.exists(), "its own now-empty project dir is removed");
    assert!(unrelated.exists(), "directories it does not own stay");
    assert!(home.join("projects").exists());
    let manifest = read(
        &f.paths
            .root
            .join(sessionsation::layout::ARCHIVED_SESSIONS)
            .join(session["id"].as_str().unwrap())
            .join("native.json"),
    );
    let m: Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(m["state"], "deleted");
    assert_eq!(m["session"]["id"], session["id"]);
    assert_eq!(m["session"]["name"], "");
    assert!(m["session"]["directory"].is_null(), "{manifest}");
    assert!(m["session"]["attachment"].is_null());
    assert_eq!(m["home"], "");
    assert!(
        !manifest.contains(f.project.to_str().unwrap()),
        "{manifest}"
    );
}

#[path = "support/fake_launch_host.rs"]
mod fake_launch_host;

/// The UI path (`api launch start` -> HERDR pane -> `launch-run`) with no
/// `launch_permissions` in the data-root config starts Claude in yolo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_launch_without_configured_permissions_starts_claude_in_yolo() {
    let f = fixture().await;
    let config = f.paths.root.join("config.json");
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    saved.as_object_mut().unwrap().remove("launch_permissions");
    std::fs::write(&config, saved.to_string()).unwrap();
    f.add_logged_in("personal");
    let home = f.home("personal");
    let dir = f.paths.root.parent().unwrap().to_owned();
    let _pane = fake_launch_host::launch(binary(), &f.paths.root, &dir, "claude:personal", "ui");
    wait_for("claude launch", || home.join("fake-launch-new").exists());
    let launch = read(&home.join("fake-launch-new"));
    assert!(
        launch.contains("bypass=1\n") && launch.contains("is_sandbox=1\n"),
        "{launch}"
    );
    assert!(
        launch.contains(&format!("cwd={}", dir.join("ui").display())),
        "{launch}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypass_warning_defaults_to_skip_and_toggles_one_key() {
    let f = fixture().await;
    let add = f.run(&[
        "account",
        "add",
        "--platform",
        "claude",
        "--no-login",
        "personal",
    ]);
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let home = f.home("personal");
    // New managed account: default skip, written to settings.json.
    let settings: Value = serde_json::from_str(&read(&home.join("settings.json"))).unwrap();
    assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
    assert_eq!(settings["statusLine"]["type"], "command");
    let shown = f.api(&["account", "bypass-warning", "claude:personal"]);
    assert_eq!(shown["bypass_warning"], "skip");
    assert_eq!(shown["changed"], false);
    let list = f.api(&["account", "list", "--platform", "claude"]);
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "personal")
        .unwrap();
    assert_eq!(row["bypass_warning"], "skip");
    assert_eq!(row["bypass_warning_value"], true);

    // Show: explicit false, other keys preserved.
    std::fs::write(
        home.join("settings.json"),
        r#"{"theme":"dark","skipDangerousModePermissionPrompt":true}"#,
    )
    .unwrap();
    let out = f.api(&["account", "bypass-warning", "claude:personal", "--show"]);
    assert_eq!(out["changed"], true);
    assert_eq!(out["bypass_warning"], "show");
    let settings: Value = serde_json::from_str(&read(&home.join("settings.json"))).unwrap();
    assert_eq!(
        settings,
        json!({"theme":"dark","skipDangerousModePermissionPrompt":false})
    );
    let again = f.api(&["account", "bypass-warning", "claude:personal", "--show"]);
    assert_eq!(again["changed"], false);
    let out = f.api(&["account", "bypass-warning", "claude:personal", "--skip"]);
    assert_eq!(out["bypass_warning"], "skip");
    assert_eq!(out["previous"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_last_accessed_is_the_last_transcript_record_and_null_without_a_source() {
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
    let id = uuid::Uuid::new_v4().to_string();
    let path = transcript(&home, &f.project, &id);
    // Creation time is 2026-01-01; the last record decides.
    append(
        &path,
        json!({"type":"assistant","timestamp":"2026-10-29T10:05:00.000Z","message":{"content":"x"}}),
    );
    append(&path, json!({"type":"file-history-snapshot"}));
    let s = by_conversation(&f, &id);
    assert_eq!(s["last_accessed"], 1_793_268_300_000_i64, "{s}");
    assert_eq!(s["conversation_created_at"], "2026-01-01T00:00:00.000Z");
    // A later record moves it; the cache is keyed on the file's stamp.
    append(
        &path,
        json!({"type":"user","timestamp":"2026-10-29T11:00:00.000Z"}),
    );
    assert_eq!(
        by_conversation(&f, &id)["last_accessed"],
        1_793_271_600_000_i64
    );
    // No source: a catalog entry whose transcript is gone reports null.
    std::fs::remove_file(&path).unwrap();
    if let Some(s) = f
        .claude_sessions()
        .into_iter()
        .find(|s| s["conversation_id"] == id.as_str())
    {
        assert!(s["last_accessed"].is_null(), "{s}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_managed_home_gets_default_statusline_but_adopted_and_existing_do_not() {
    let f = fixture().await;
    let add = |args: &[&str]| {
        let mut a = vec!["account", "add", "--platform", "claude", "--no-login"];
        a.extend_from_slice(args);
        let out = f.run(&a);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let expected =
        json!({"type":"command","command":"bash /mnt/user/appdata/claude/statusline-command.sh"});
    // New managed home: seeded next to the bypass default.
    add(&["fresh"]);
    let settings: Value =
        serde_json::from_str(&read(&f.home("fresh").join("settings.json"))).unwrap();
    assert_eq!(settings["statusLine"], expected);
    assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
    // Adopted directory: untouched.
    let adopted = f._tmp.path().join("adopted-claude");
    std::fs::create_dir(&adopted).unwrap();
    add(&["--config-dir", adopted.to_str().unwrap(), "adopted"]);
    assert!(!adopted.join("settings.json").exists());
    // Existing operator-populated managed home: untouched.
    let existing = f.home("existing");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o700)).unwrap();
    add(&["existing"]);
    assert!(!existing.join("settings.json").exists());
    // Empty value disables seeding.
    let set = f.run(&["config", "set", "claude_default_statusline_command", ""]);
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    add(&["plain"]);
    let settings: Value =
        serde_json::from_str(&read(&f.home("plain").join("settings.json"))).unwrap();
    assert!(settings.get("statusLine").is_none(), "{settings}");
    assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
    // A custom command is used verbatim.
    let set = f.run(&[
        "config",
        "set",
        "claude_default_statusline_command",
        "bash /x/y.sh",
    ]);
    assert!(set.status.success());
    add(&["custom"]);
    let settings: Value =
        serde_json::from_str(&read(&f.home("custom").join("settings.json"))).unwrap();
    assert_eq!(
        settings["statusLine"],
        json!({"type":"command","command":"bash /x/y.sh"})
    );
}
