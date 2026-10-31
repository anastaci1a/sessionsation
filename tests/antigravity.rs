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
        self.paths.root.join("accounts/antigravity").join(name)
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
    // Names are unique per provider: the same name is free under another provider
    // but not twice under this one.
    let taken = f.api_error(&["account", "add", "antigravity:fresh"]);
    assert!(taken.contains("ACCOUNT_NAME_EXISTS"), "{taken}");
    f.api(&["account", "add", "gemini:fresh"]);
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
    assert_eq!(caps["contract_revision"], 32);
    assert_eq!(caps["contract_version"], "1.32.0");
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
    // Last activity comes from the conversation files (honest mtime), never the call time.
    let accessed = s["last_accessed"]
        .as_i64()
        .expect("last_accessed populated");
    assert!(
        accessed > 1_700_000_000_000 && accessed <= now() * 1000 + 5000,
        "{s}"
    );
    assert_eq!(s["capabilities"]["archive"], true, "{s}");
    assert_eq!(s["capabilities"]["delete"], true);
    assert_eq!(s["capabilities"]["stop"], false);
    assert_eq!(s["capability_reasons"]["stop"], "SESSION_NOT_RUNNING");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_version_reports_semver_and_negotiates_by_major() {
    let f = fixture().await;
    let v = f.api(&["version"]);
    assert_eq!(v["contract_version"], "1.32.0");
    assert_eq!(v["contract_revision"], 32);
    for ok in ["1", "1.25", "1.26", "1.99.0"] {
        assert_eq!(
            f.api(&["--api-version", ok, "version"])["contract_version"],
            "1.32.0"
        );
    }
    for bad in ["2", "2.0.0", "0.24"] {
        let err = f.api_error(&["--api-version", bad, "version"]);
        assert!(
            err.contains("unsupported API major version"),
            "{bad}: {err}"
        );
    }
}

// ------------------------------------------------- revision 25: lifecycle

const INDEX_SCHEMA: &str = "CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`workspace_uris` text NOT NULL,`raw_summary` blob,PRIMARY KEY (`conversation_id`))";

fn map_entry(id: &str, body: &[u8]) -> Vec<u8> {
    let mut inner = vec![0x0a, id.len() as u8];
    inner.extend_from_slice(id.as_bytes());
    inner.extend_from_slice(&[0x12, body.len() as u8]);
    inner.extend_from_slice(body);
    let mut out = vec![0x0a, inner.len() as u8];
    out.extend(inner);
    out
}
fn state_files(home: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                walk(&p, out);
            } else if !p.to_string_lossy().contains("conversation_summaries.db") {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = vec![];
    walk(&home.join(".gemini/antigravity-cli"), &mut out);
    out.sort();
    out
}
fn index_rows(home: &Path) -> Vec<(String, String, Vec<u8>)> {
    let db =
        rusqlite::Connection::open(home.join(".gemini/antigravity-cli/conversation_summaries.db"))
            .unwrap();
    let mut st = db
        .prepare(
            "SELECT conversation_id, title, raw_summary FROM conversation_summaries ORDER BY 1",
        )
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}
impl Fixture {
    fn conversation(&self, account: &str, home: &Path) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.send_ok(&new_body(&id, account, &self.project, "start"));
        let native = self.settled(&id)["provider_conversation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let state = home.join(".gemini/antigravity-cli");
        std::fs::create_dir_all(state.join("annotations")).unwrap();
        std::fs::write(
            state.join("annotations").join(format!("{native}.pbtxt")),
            "title:\"x\"",
        )
        .unwrap();
        std::fs::write(
            state.join("brain").join(&native).join("scratch.txt"),
            "work",
        )
        .unwrap();
        std::fs::write(
            state.join("conversations").join(format!("{native}.db-wal")),
            "wal",
        )
        .unwrap();
        let db = rusqlite::Connection::open(state.join("conversation_summaries.db")).unwrap();
        if db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='conversation_summaries'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 0
        {
            db.execute_batch(INDEX_SCHEMA).unwrap();
        }
        db.execute(
            "INSERT INTO conversation_summaries VALUES (?1, 'title', 1, '2026-10-29 21:52:00+00:00', '[]', ?2)",
            rusqlite::params![native, vec![0u8, 255, 7, native.len() as u8]],
        )
        .unwrap();
        native
    }
    fn job(&self, command: &str, session: &Value) -> Value {
        let request = uuid::Uuid::new_v4().to_string();
        let id = session["id"].as_str().unwrap();
        self.api(&[
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
        let mut last = Value::Null;
        wait_for(command, || {
            last = self
                .api(&["operations"])
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["id"] == request.as_str())
                .cloned()
                .unwrap_or(Value::Null);
            matches!(last["phase"].as_str(), Some("complete" | "failed"))
        });
        last
    }
    fn session_by_id(&self, id: &str) -> Option<Value> {
        self.api(&["state", "--archive-filter", "all"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_restore_and_delete_move_only_the_exact_conversation_and_stop_ends_it() {
    let f = fixture().await;
    f.api(&["account", "add", "antigravity:life"]);
    assert!(
        f.run(&["account", "login", "antigravity:life"])
            .status
            .success()
    );
    f.api(&["account", "refresh", "antigravity:life"]);
    let home = f.home("life");
    let state = home.join(".gemini/antigravity-cli");
    let mine = f.conversation("antigravity:life", &home);
    let other = f.conversation("antigravity:life", &home);
    std::fs::write(state.join("presence").join(format!("{mine}.lock")), "").unwrap();
    std::fs::create_dir_all(state.join("implicit")).unwrap();
    std::fs::write(state.join("implicit/shared.pb"), "implicit").unwrap();
    std::fs::write(
        state.join("history.jsonl"),
        format!("{{\"conversationId\":\"{mine}\"}}\n"),
    )
    .unwrap();
    let (jetbox_mine, jetbox_other) = (map_entry(&mine, b"abc"), map_entry(&other, b"defgh"));
    std::fs::write(
        state.join("jetbox_summaries_proto.pb"),
        [jetbox_other.clone(), jetbox_mine.clone()].concat(),
    )
    .unwrap();
    let rows_before = index_rows(&home);
    assert_eq!(rows_before.len(), 2);
    let before = state_files(&home);
    let s = f.by_conversation(&mine);
    assert_eq!(s["capabilities"]["archive"], true, "{s}");
    assert_eq!(s["capabilities"]["restore"], false);
    let caps = f.api(&["capabilities"]);
    let agy = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "antigravity")
        .unwrap()
        .clone();
    assert_eq!(agy["stop"], true);
    assert_eq!(agy["operations"]["archive"]["supported"], true);
    assert_eq!(agy["operations"]["delete_session"]["supported"], true);
    assert_eq!(agy["operations"]["stop"]["supported"], true);
    assert_eq!(agy["operations"]["restart"]["supported"], false);
    assert_eq!(caps["antigravity_lifecycle"]["stop"]["sigkill"], false);
    assert_eq!(caps["antigravity_folder_trust"]["credentials_read"], false);

    // Archive: exactly this id leaves the home; the other conversation, the token,
    // history and implicit files stay; the index row is removed and saved.
    let op = f.job("archive", &s);
    assert_eq!(op["phase"], "complete", "{op}");
    let after = state_files(&home);
    for (p, _) in &after {
        assert!(
            !p.to_string_lossy().contains(&mine) || p.starts_with(state.join("log")),
            "{}",
            p.display()
        );
    }
    assert!(
        after
            .iter()
            .any(|(p, _)| p.ends_with("antigravity-oauth-token"))
    );
    assert!(
        after
            .iter()
            .any(|(p, b)| p.ends_with("implicit/shared.pb") && b == b"implicit")
    );
    assert!(
        before
            .iter()
            .filter(|(p, _)| !p.to_string_lossy().contains(&mine)
                && !p.ends_with("jetbox_summaries_proto.pb"))
            .all(|e| after.contains(e)),
        "other files are byte-identical"
    );
    let rows = index_rows(&home);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, other);
    assert_eq!(
        std::fs::read(state.join("jetbox_summaries_proto.pb")).unwrap(),
        [jetbox_other.clone(), jetbox_mine.clone()].concat(),
        "archive leaves agy's summary cache alone"
    );
    let archived = f.session_by_id(s["id"].as_str().unwrap()).unwrap();
    assert_eq!(archived["availability"], "archived", "{archived}");
    assert_eq!(archived["capabilities"]["restore"], true);
    assert_eq!(archived["capabilities"]["archive"], false);
    assert!(
        f.api(&["archives"])
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == s["id"])
    );

    // Restore puts every file and the whole index row back.
    let op = f.job("restore-session", &archived);
    assert_eq!(op["phase"], "complete", "{op}");
    assert_eq!(state_files(&home), before);
    assert_eq!(index_rows(&home), rows_before);

    // A live agy blocks archive and delete, and Stop ends it by SIGTERM.
    let child = Process(
        Command::new(&f.fake)
            .args(["--conversation", &mine])
            .env("HOME", &home)
            .env("AGY_ACCOUNT_HOME", &home)
            .current_dir(&f.project)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for("live session", || {
        f.by_conversation(&mine)["availability"] == "running"
    });
    let live = f.by_conversation(&mine);
    assert_eq!(live["capabilities"]["archive"], false);
    assert_eq!(live["capabilities"]["delete"], false);
    assert_eq!(live["capabilities"]["stop"], true, "{live}");
    assert!(
        live["capability_reasons"]["archive"]
            .as_str()
            .unwrap()
            .contains("SESSION_RUNNING")
    );
    let refused = f.api_error(&[
        "delete-session",
        "--session",
        live["id"].as_str().unwrap(),
        "--expected-generation",
        live["generation"].as_str().unwrap(),
        "--request-id",
        &uuid::Uuid::new_v4().to_string(),
        "--confirm",
        live["id"].as_str().unwrap(),
    ]);
    assert!(refused.contains("SESSION_RUNNING"), "{refused}");
    assert_eq!(
        state_files(&home),
        before,
        "a refused delete changes nothing"
    );
    let stopped = f.api(&[
        "stop",
        "--session",
        live["id"].as_str().unwrap(),
        "--expected-generation",
        live["generation"].as_str().unwrap(),
    ]);
    assert_eq!(stopped["phase"], "complete", "{stopped}");
    assert_eq!(stopped["signal"], "SIGTERM");
    assert_eq!(stopped["pid"], child.0.id());
    wait_for("stopped", || {
        f.by_conversation(&mine)["availability"] == "stopped"
    });
    drop(child);
    let again = f.by_conversation(&mine);
    assert!(
        f.api_error(&[
            "stop",
            "--session",
            again["id"].as_str().unwrap(),
            "--expected-generation",
            again["generation"].as_str().unwrap(),
        ])
        .contains("SESSION_NOT_RUNNING")
    );

    // Delete removes the files, the row, the cache entry and the catalog entry.
    let op = f.job("delete-session", &again);
    assert_eq!(op["phase"], "complete", "{op}");
    for (p, _) in state_files(&home) {
        assert!(
            !p.to_string_lossy().contains(&mine) || p.starts_with(state.join("log")),
            "{}",
            p.display()
        );
    }
    assert_eq!(index_rows(&home).len(), 1);
    assert_eq!(
        std::fs::read(state.join("jetbox_summaries_proto.pb")).unwrap(),
        jetbox_other
    );
    assert!(f.session_by_id(again["id"].as_str().unwrap()).is_none());
    assert!(
        f.sessions()
            .iter()
            .any(|x| x["conversation_id"] == other.as_str())
    );
    assert_eq!(
        read(&state.join("history.jsonl")),
        format!("{{\"conversationId\":\"{mine}\"}}\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_starts_pre_trust_the_launch_directory_and_the_setting_turns_it_off() {
    let f = fixture().await;
    f.api(&["account", "add", "antigravity:trust"]);
    assert!(
        f.run(&["account", "login", "antigravity:trust"])
            .status
            .success()
    );
    let home = f.home("trust");
    let settings = home.join(".gemini/antigravity-cli/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{\n  \"queuedMessages\": \"queue\"\n}\n").unwrap();
    let run = |permissions: &str| {
        let mut c = f.command(&[
            "--permissions",
            permissions,
            "run",
            "--account",
            "antigravity:trust",
        ]);
        c.current_dir(&f.project);
        c.output().unwrap()
    };
    // The fake interactive agy exits 1 on EOF, so only its effects are checked.
    let out = run("default");
    assert!(
        read(&home.join("fake-agy-calls")).contains("--conversation"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !read(&settings).contains("trustedWorkspaces"),
        "default permissions do not pre-trust"
    );
    run("yolo");
    let value: Value = serde_json::from_str(&read(&settings)).unwrap();
    assert_eq!(value["queuedMessages"], "queue");
    assert_eq!(
        value["trustedWorkspaces"],
        json!([f.project.to_str().unwrap()])
    );

    std::fs::write(&settings, "{}").unwrap();
    let mut config = sessionsation::store::load_config(&f.paths).unwrap();
    config.antigravity_pretrust_folders = false;
    sessionsation::store::save_config(&f.paths, &config).unwrap();
    run("yolo");
    assert_eq!(read(&settings), "{}");
}
