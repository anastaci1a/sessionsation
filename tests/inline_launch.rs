#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
#![allow(dead_code)]
//! Inline run/resume journaling and --name (contract 1.32), driven through the
//! Claude fake (`support/fake-claude.sh`). Disposable homes only; no provider
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
    fn sessions_of(&self, provider: &str) -> Vec<Value> {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["provider"] == provider)
            .cloned()
            .collect()
    }
    fn claude_sessions(&self) -> Vec<Value> {
        self.sessions_of("claude")
    }
    fn session(&self, id: &str) -> Value {
        self.claude_sessions()
            .into_iter()
            .find(|s| s["id"] == id)
            .unwrap()
    }
    fn by_conversation(&self, native: &str) -> Value {
        self.claude_sessions()
            .into_iter()
            .find(|s| s["conversation_id"] == native)
            .unwrap_or_else(|| panic!("no claude session for {native}"))
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("accounts/claude").join(name)
    }
    /// Enroll a Claude account (the fake reports a subscription login) and refresh it.
    fn add_logged_in(&self, name: &str) {
        self.api(&["account", "add", &format!("claude:{name}")]);
        let r = self.api(&["account", "refresh", &format!("claude:{name}")]);
        assert_eq!(r["auth_required"], false, "{r}");
    }
    /// A stopped session discovered from a chat file in the account home.
    fn stopped_session(&self, account: &str) -> (String, String) {
        let native = uuid::Uuid::new_v4().to_string();
        transcript(&self.home(account), &self.project, &native);
        let s = self.by_conversation(&native);
        assert_eq!(s["availability"], "stopped");
        (s["id"].as_str().unwrap().to_owned(), native)
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
fn slug(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}
/// A stopped Claude conversation: one transcript record under its project key.
fn transcript(home: &Path, cwd: &Path, id: &str) -> PathBuf {
    let dir = home.join("projects").join(slug(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type":"user","sessionId":id,"cwd":cwd,"timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"private prompt"}})
        ),
    )
    .unwrap();
    path
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
    assert_eq!(caps["contract_version"], "1.73.4");
    assert_eq!(caps["session_compact"]["command"], "session compact");
    assert_eq!(
        caps["session_compact"]["provider"],
        serde_json::json!(["codex", "claude"])
    );
    assert_eq!(caps["contract_revision"], 73);
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
    wait_for("fake claude launch", || {
        home.join("fake-launch-new").exists()
    });
    // The catalog entry carries the typed name, not the directory name.
    wait_for("named catalog session", || {
        f.claude_sessions()
            .iter()
            .any(|s| s["name"] == "typed name")
    });
    assert_eq!(f.claude_sessions().len(), 1);
    let op = inline_op(&f, "launch", "typed name");
    assert!(
        matches!(op["phase"].as_str(), Some("attached" | "complete")),
        "{op}"
    );
    assert_eq!(op["account_id"], "claude:personal");
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
        inline_op(&f, "launch", "typed name")["phase"] == "complete"
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
    wait_for("fake claude launch", || {
        home.join("fake-launch-new").exists()
    });
    let op = inline_op(&f, "launch", "project");
    assert_eq!(f.claude_sessions()[0]["name"], "project");
    assert_eq!(op["session_id"], f.claude_sessions()[0]["id"]);
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
    assert!(f.claude_sessions().is_empty());
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
    f.api(&["account", "add", "claude:fresh"]);
    std::fs::write(f.home("fresh").join("fake-auth-mode"), "logged_out").unwrap();
    f.api(&["account", "refresh", "claude:fresh"]);
    let refused = f
        .command(&["run", "--account", "fresh", "--name", "later"])
        .current_dir(&f.project)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&refused.stderr).contains("AUTH_REQUIRED"));
    let op = inline_op(&f, "launch", "later");
    assert_eq!(op["phase"], "failed");
    assert!(op["detail"].as_str().unwrap().contains("AUTH_REQUIRED"));
    std::fs::write(f.home("fresh").join("fake-auth-mode"), "subscription").unwrap();
    f.api(&["account", "refresh", "claude:fresh"]);
    let child = f
        .command(&["run", "--account", "fresh", "--name", "later"])
        .current_dir(&f.project)
        .spawn()
        .unwrap();
    wait_for("named session after the failed attempt", || {
        f.claude_sessions().iter().any(|s| s["name"] == "later")
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
    wait_for("fake claude resume", || {
        home.join("fake-launch-resume").exists()
    });
    assert!(read(&home.join("fake-launch-resume")).contains(&format!("session={native}")));
    assert_eq!(f.session(&id)["name"], "renamed by resume");
    let op = inline_op(&f, "resume", "renamed by resume");
    assert_eq!(op["session_id"], id.as_str());
    assert!(
        matches!(op["phase"].as_str(), Some("attached" | "complete")),
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
        inline_op(&f, "resume", "renamed by resume")["phase"] == "complete"
    });
    // Process exit: the same terminal phase, an honest detail, and abandon agrees.
    let op = inline_op(&f, "resume", "renamed by resume");
    assert_eq!(op["detail"], "The inline provider process has ended.");
    let oid = op["id"].as_str().unwrap();
    assert!(
        f.api_error(&["abandon-operation", "--operation", oid, "--confirm", oid])
            .contains("OPERATION_ALREADY_TERMINAL")
    );
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
                "account_id":"claude:personal","name":"held","host":{"session":"s","socket":"/s"},
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finished_inline_entries_of_a_deleted_session_or_long_ago_leave_operations() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let (id, native) = f.stopped_session("personal");
    let dir = f.paths.root.join(sessionsation::layout::RESUMPTIONS);
    std::fs::create_dir_all(&dir).unwrap();
    let write = |session: &str, name: &str, updated: i64| {
        let rid = uuid::Uuid::new_v4().to_string();
        // Dead process and gone pane: pid 0x7fffffff never exists.
        std::fs::write(
            dir.join(format!("{rid}.json")),
            serde_json::to_vec(&json!({
                "version":1,
                "request":{"version":1,"request_id":rid,"session_id":session,"expected_generation":"g",
                    "account_id":"claude:personal","name":name,"host":{"session":"inline","socket":"/"},
                    "space":{"kind":"existing","id":"inline"}},
                "directory":"/","previous_name":"p","conversation_id":native,"generation":"g",
                "phase":"running","detail":"Exact provider process observed; no launch replay",
                "workspace_id":null,"tab_id":null,"pane_id":"w9:p9","operation_id":null,
                "created_at":updated,"updated_at":updated,
                "inline":{"pid":2147483647u32,"process_start":1,"pane_id":"w9:p9"}}))
            .unwrap(),
        )
        .unwrap();
        rid
    };
    let gone = write("00000000-0000-4000-8000-000000000000", "deleted", now());
    let old = write(&id, "old", 1);
    let recent = write(&id, "recent", now());
    let names: Vec<_> = operations(&f).iter().map(|o| o["name"].clone()).collect();
    assert_eq!(names, vec![json!("recent")], "{names:?}");
    assert_eq!(
        inline_op(&f, "resume", "recent")["detail"],
        "The inline provider process has ended."
    );
    // Receipts stay for history, and abandon agrees that they are terminal.
    for rid in [&gone, &old, &recent] {
        assert!(dir.join(format!("{rid}.json")).exists());
        assert!(
            f.api_error(&["abandon-operation", "--operation", rid, "--confirm", rid])
                .contains("OPERATION_ALREADY_TERMINAL")
        );
    }
}

fn proc_start(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split(' ').nth(19).unwrap().parse().unwrap()
}
#[allow(clippy::too_many_arguments)]
fn write_inline_receipt(
    f: &Fixture,
    session: &str,
    native: &str,
    name: &str,
    phase: &str,
    detail: Option<&str>,
    pid: u32,
    start: u64,
) -> String {
    let dir = f.paths.root.join(sessionsation::layout::RESUMPTIONS);
    std::fs::create_dir_all(&dir).unwrap();
    let rid = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        dir.join(format!("{rid}.json")),
        serde_json::to_vec(&json!({
            "version":1,
            "request":{"version":1,"request_id":rid,"session_id":session,"expected_generation":"g",
                "account_id":"claude:personal","name":name,"host":{"session":"inline","socket":"/"},
                "space":{"kind":"existing","id":"inline"}},
            "directory":"/","previous_name":"p","conversation_id":native,"generation":"g",
            "phase":phase,"detail":detail,
            "workspace_id":null,"tab_id":null,"pane_id":"w9:p9","operation_id":null,
            "created_at":now(),"updated_at":now(),
            "inline":{"pid":pid,"process_start":start,"pane_id":"w9:p9"}}))
        .unwrap(),
    )
    .unwrap();
    rid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_receipt_with_a_live_process_settles_to_complete_and_is_not_in_progress() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let (id, native) = f.stopped_session("personal");
    let mut child = Command::new("sleep").arg("300").spawn().unwrap();
    let rid = write_inline_receipt(
        &f,
        &id,
        &native,
        "live old",
        "running",
        Some("Exact provider process observed; no launch replay"),
        child.id(),
        proc_start(child.id()),
    );
    let op = inline_op(&f, "resume", "live old");
    assert_eq!(op["phase"], "complete", "{op}");
    assert_eq!(
        op["detail"],
        "Exact provider process observed; no launch replay"
    );
    let receipt = f.api(&["resume-launch", "status", "--request-id", &rid]);
    assert_eq!(receipt["phase"], "complete");
    // Terminal for abandon, and no longer an unfinished resumption of the session.
    assert!(
        f.api_error(&["abandon-operation", "--operation", &rid, "--confirm", &rid])
            .contains("OPERATION_ALREADY_TERMINAL")
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inline_process_that_dies_before_attaching_settles_failed() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let (id, native) = f.stopped_session("personal");
    let rid = write_inline_receipt(
        &f,
        &id,
        &native,
        "never attached",
        "starting_provider",
        None,
        2147483647,
        1,
    );
    let op = inline_op(&f, "resume", "never attached");
    assert_eq!(op["phase"], "failed", "{op}");
    let receipt = f.api(&["resume-launch", "status", "--request-id", &rid]);
    assert_eq!(receipt["phase"], "failed");
}

fn tmp_line(home: &Path) -> String {
    read(&home.join("fake-launch-new"))
        .lines()
        .find_map(|l| l.strip_prefix("tmp=").map(str::to_owned))
        .unwrap()
}
fn launch_claude(f: &Fixture, name: &str) -> String {
    let home = f.home("personal");
    let child = f
        .command(&["run", "--account", "personal", "--name", name])
        .current_dir(&f.project)
        .spawn()
        .unwrap();
    wait_for("fake claude launch", || {
        home.join("fake-launch-new").exists()
    });
    let line = tmp_line(&home);
    reap(child);
    line
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_gets_a_private_per_account_temp_directory_unless_none() {
    // Contract 1.72.1: TMPDIR, TMP, TEMP and CLAUDE_CODE_TMPDIR; "none" keeps the environment.
    let f = fixture().await;
    f.add_logged_in("personal");
    let dir = f.paths.root.join("tmp/claude/personal");
    let d = dir.display();
    assert_eq!(launch_claude(&f, "one"), format!("{d},{d},{d},{d}"));
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::fs::remove_file(f.home("personal").join("fake-launch-new")).unwrap();
    let path = f.paths.root.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["session_tmp_root"] = json!("none");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(launch_claude(&f, "two"), ",,,");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_temp_directory_refuses_the_launch() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let elsewhere = f._tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(f.paths.root.join("tmp/claude")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, f.paths.root.join("tmp/claude/personal")).unwrap();
    let out = f.run(&["run", "--account", "personal"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("SESSION_TMP_UNSAFE"));
    assert!(!f.home("personal").join("fake-launch-new").exists());
}
