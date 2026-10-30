//! Gemini terminal input (contract revision 17) against fakes only:
//! `support/fake-herdr-gemini.py` (HERDR with one pane showing a fake Gemini
//! CLI prompt) and `support/fake-gemini.sh` (the live Gemini process the pane
//! runs). Disposable homes; no real pane, provider, account or model is touched.
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    io::Write,
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
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
struct Fixture {
    _tmp: tempfile::TempDir,
    paths: Paths,
    project: PathBuf,
    bin: PathBuf,
    herdr: PathBuf,
    fake: PathBuf,
    _daemon: Process,
}
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let install = |path: PathBuf, body: &str| {
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let fake = install(
        tmp.path().join("gemini.js"),
        include_str!("support/fake-gemini.sh"),
    );
    install(
        bin.join("herdr"),
        include_str!("support/fake-herdr-gemini.py"),
    );
    let codex = install(
        tmp.path().join("fake-codex"),
        include_str!("support/fake-codex.sh"),
    );
    let claude = install(
        tmp.path().join("fake-claude"),
        include_str!("support/fake-claude.sh"),
    );
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
        gemini_binary: fake.to_str().unwrap().into(),
        ..Default::default()
    };
    sessionsation::store::save_config(&paths, &config).unwrap();
    let d = Process(
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
    let herdr = tmp.path().join("herdr-state");
    std::fs::create_dir(&herdr).unwrap();
    Fixture {
        _tmp: tmp,
        paths,
        project,
        bin,
        herdr,
        fake,
        _daemon: d,
    }
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
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("GEMINI_CLI_HOME")
            .env(
                "PATH",
                format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("FAKE_HERDR_DIR", &self.herdr)
            .env("GEMINI_API_KEY", "key-must-not-reach-gemini")
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
    fn home(&self) -> PathBuf {
        self.paths.root.join("accounts/gemini").join("personal")
    }
    fn session(&self, native: &str) -> Value {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["conversation_id"] == native || s["id"] == native)
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn send(&self, body: &Value) -> Output {
        let mut child = self
            .command(&["api", "send"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
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
        wait_for("terminal receipt", || {
            last = self.api(&["receipt", message]);
            last["execution"]["phase"] == "complete"
        });
        last
    }
    fn tui(&self) -> Value {
        serde_json::from_str(&read(&self.herdr.join("tui.json"))).unwrap()
    }
    fn set_tui(&self, v: &Value) {
        std::fs::write(self.herdr.join("tui.json"), v.to_string()).unwrap();
    }
    fn writes(&self) -> Vec<Vec<String>> {
        read(&self.herdr.join("calls"))
            .lines()
            .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
            .filter(|a| a.iter().any(|x| x == "send-text" || x == "send-keys"))
            .collect()
    }
    fn steering(&self, flag: &str) -> Value {
        let out = self.run(&["account", "live-steering", "gemini:personal", flag]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    /// The interactive Gemini process running in HERDR pane w1:p1.
    fn gemini(&self, flag: &str, native: &str) -> Process {
        Process(
            Command::new(&self.fake)
                .args([flag, native])
                .env("GEMINI_CLI_HOME", self.home())
                .env("HERDR_PANE_ID", "w1:p1")
                .env("HERDR_SESSION", "fixture")
                .env("HERDR_ENV", "1")
                .env_remove("GEMINI_CLI_SYSTEM_SETTINGS_PATH")
                .env(
                    "GEMINI_CLI_SYSTEM_SETTINGS_PATH",
                    self.project.join("no-system-settings.json"),
                )
                .env(
                    "GEMINI_CLI_SYSTEM_DEFAULTS_PATH",
                    self.project.join("no-system-defaults.json"),
                )
                .current_dir(&self.project)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}
fn body(mode: &str, session: &Value, text: &str) -> Value {
    json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":mode,"session_id":session["id"],
        "expected_generation":session["generation"],"text":text,"expires_at":daemon::now()+600})
}
fn records(transcript: &Path) -> Vec<Value> {
    read(transcript)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
fn chat_file(home: &Path, native: &str) -> PathBuf {
    find_chat(home, native).unwrap_or_else(|| panic!("no chat for {native}"))
}
fn find_chat(home: &Path, native: &str) -> Option<PathBuf> {
    let suffix = format!("-{}.jsonl", &native[..8]);
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
    found.pop()
}

#[test]
fn the_account_toggle_edits_only_model_steering() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture());
    let out = f.run(&[
        "account",
        "add",
        "--platform",
        "gemini",
        "--no-login",
        "personal",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let settings = f.home().join(".gemini/settings.json");
    let seeded: Value = serde_json::from_str(&read(&settings)).unwrap();
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o640)).unwrap();
    let shown = f.steering("--disable");
    assert_eq!(shown["changed"], false);
    let on = f.steering("--enable");
    assert_eq!(on["provider"], "gemini");
    assert_eq!(on["setting"], "experimental.modelSteering");
    assert_eq!(on["live_steering"], true);
    assert_eq!(on["changed"], true);
    assert_eq!(on["restart_required"], false, "no session running");
    assert_eq!(f.steering("--enable")["changed"], false);
    let mut want = seeded.clone();
    want["experimental"] = json!({"modelSteering":true});
    assert_eq!(
        serde_json::from_str::<Value>(&read(&settings)).unwrap(),
        want
    );
    assert_eq!(
        std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o640,
        "file mode kept"
    );
    let off = f.steering("--disable");
    assert_eq!(
        (off["changed"].clone(), off["live_steering"].clone()),
        (json!(true), json!(false))
    );
    want["experimental"] = json!({});
    assert_eq!(
        serde_json::from_str::<Value>(&read(&settings)).unwrap(),
        want
    );
    // A symlinked settings file is refused and left alone.
    let real = f.home().join("real-settings.json");
    std::fs::rename(&settings, &real).unwrap();
    std::os::unix::fs::symlink(&real, &settings).unwrap();
    let out = f.run(&["account", "live-steering", "gemini:personal", "--enable"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("SETTINGS_IS_SYMLINK"));
    assert_eq!(serde_json::from_str::<Value>(&read(&real)).unwrap(), want);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_terminal_input_submits_queues_and_steers_as_the_user() {
    let f = fixture().await;
    let out = f.run(&[
        "account",
        "add",
        "--platform",
        "gemini",
        "--no-login",
        "personal",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let creds = f.home().join(".gemini");
    std::fs::write(creds.join("oauth_creds.json"), "synthetic fixture").unwrap();
    std::fs::write(
        creds.join("google_accounts.json"),
        r#"{"active":"fixture@example.invalid","old":[]}"#,
    )
    .unwrap();
    assert_eq!(
        f.api(&["account", "refresh", "gemini:personal"])["auth_required"],
        false
    );
    let native = uuid::Uuid::new_v4().to_string();
    let process = f.gemini("--session-id", &native);
    wait_for("chat file", || {
        find_chat(&f.home(), &native).is_some_and(|p| read(&p).contains("sessionId"))
    });
    let transcript = chat_file(&f.home(), &native);
    let base = json!({"pane":"w1:p1","pid":process.0.id(),"transcript":transcript});
    let reset = |patch: Value| {
        let mut v = base.clone();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        f.set_tui(&v);
    };
    reset(json!({}));
    let mut s = Value::Null;
    wait_for("attached live gemini session", || {
        s = f.session(&native);
        s["availability"] == "running" && s["attachment"]["host"] == "herdr"
    });
    let id = s["id"].as_str().unwrap().to_owned();
    // Model steering off: submit and queue typed, steer refused with its reason.
    assert_eq!(s["input_delivery"]["semantics"], "terminal_input", "{s}");
    assert_eq!(s["input_delivery"]["modes"]["queue"], "terminal_input");
    assert_eq!(s["input_delivery"]["modes"]["steer"], Value::Null);
    assert_eq!(s["input_delivery"]["model_steering"]["enabled"], false);
    assert_eq!(s["capabilities"]["submit"], true);
    assert_eq!(s["capabilities"]["steer"], false);
    assert!(
        s["capability_reasons"]["steer"]
            .as_str()
            .unwrap()
            .starts_with("GEMINI_STEERING_DISABLED")
    );
    assert!(
        f.send_err(&body("steer", &s, "x"))
            .contains("GEMINI_STEERING_DISABLED")
    );
    let mut interrupt = body("submit", &s, "x");
    interrupt["steering"] = json!({"version":1,"interrupt":true});
    assert!(
        f.send_err(&interrupt)
            .contains("STEERING_ONLY_VALID_WITH_STEER")
    );
    assert!(
        f.send_err(&body("submit", &s, "see @src/main.rs"))
            .contains("TERMINAL_PAYLOAD_UNSUPPORTED")
    );
    let mut peer = body("submit", &s, "x");
    peer["delivery"] = json!("peer_message");
    assert!(f.send_err(&peer).contains("DELIVERY_UNAVAILABLE"));
    assert!(f.writes().is_empty(), "admission refusals write nothing");

    // 1. Idle submit with a two-line draft: cleared, typed, submitted, draft back.
    let draft = "half a thought\nsecond line";
    reset(json!({"input":draft}));
    let b = body("submit", &f.session(&id), "please check\nthe build.");
    let m = b["message_id"].as_str().unwrap().to_owned();
    assert_eq!(f.send_ok(&b)["delivery_semantics"], "terminal_input");
    let r = f.settled(&m);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["provider"], "gemini");
    assert_eq!(r["account_id"], "gemini:personal");
    assert_eq!(
        r["reply"]["text"],
        "terminal reply to: please check\nthe build."
    );
    let user = records(&transcript)
        .into_iter()
        .find(|x| x["content"][0]["text"] == "please check\nthe build.")
        .unwrap();
    assert_eq!(r["correlation"]["input_uuid"], user["id"]);
    assert_eq!(r["correlation"]["record_type"], "user");
    assert_eq!(r["gemini_input"]["outcome"], "submitted");
    assert_eq!(r["gemini_input"]["busy_at_submit"], false);
    assert_eq!(r["terminal_input"]["draft"]["restored"], true);
    assert_eq!(f.tui()["input"], draft, "draft back, unsubmitted");
    assert!(!r.to_string().contains("half a thought"));
    let flat: Vec<String> = f.writes().into_iter().flatten().collect();
    assert_eq!(flat.iter().filter(|x| *x == "enter").count(), 1);
    assert!(flat.iter().any(|x| x == "ctrl+j"));
    assert!(flat.iter().any(|x| x == "ctrl+u"));
    assert!(
        !f.herdr.join("forbidden").exists(),
        "no Esc/Ctrl+C/Ctrl+D/Ctrl+L"
    );
    // Same id and payload: the original receipt, nothing re-sent.
    let before = f.writes().len();
    assert_eq!(f.send_ok(&b)["state"], "completed_with_reply");
    assert_eq!(f.writes().len(), before);

    // 2. Busy queue: Gemini's native queue, joined with an earlier queued message.
    reset(json!({"status":"working","queue":["earlier queued"]}));
    let b = body("queue", &f.session(&id), "also run clippy.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["gemini_input"]["outcome"], "queued");
    assert_eq!(r["gemini_input"]["batched"], true);
    assert_eq!(r["gemini_input"]["screen_evidence"], "queued");
    assert_eq!(r["gemini_input"]["expected"], "native_queue");
    assert_eq!(r["facts"]["shared_with_turn"], true);
    assert!(
        records(&transcript)
            .iter()
            .any(|x| x["content"][0]["text"] == "earlier queued\n\nalso run clippy.")
    );

    // 3. A leading slash is escaped: plain text, not a command; help is left
    //    with backspace.
    reset(json!({"view":"help"}));
    let b = body("submit", &f.session(&id), "/compact the notes.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["terminal_input"]["escaped_prefix"], true);
    assert_eq!(r["terminal_input"]["view_switched"], "help");
    assert!(
        records(&transcript)
            .iter()
            .any(|x| x["content"][0]["text"] == "\\/compact the notes.")
    );

    // 4. Refusals before any write: nothing typed, the draft untouched.
    for (patch, code) in [
        (
            json!({"input":"[Pasted Text: 12 lines]"}),
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        (
            json!({"status":"blocked","input":"keep me"}),
            "TERMINAL_AGENT_BLOCKED",
        ),
        (json!({"view":"dialog"}), "TERMINAL_OVERLAY_UNIDENTIFIED"),
        (json!({"view":"blank"}), "TERMINAL_VIEW_UNDETERMINED"),
        (json!({"vim":true}), "TERMINAL_VIM_MODE"),
        (json!({"shell":true,"input":"ls"}), "TERMINAL_SHELL_MODE"),
        (json!({"input":"?why"}), "TERMINAL_DRAFT_UNRESTORABLE"),
    ] {
        reset(patch.clone());
        let before = f.writes().len();
        let b = body("submit", &f.session(&id), "should not arrive.");
        f.send_ok(&b);
        let r = f.settled(b["message_id"].as_str().unwrap());
        assert_eq!(r["state"], "rejected", "{patch}: {r}");
        assert_eq!(r["reason"], code, "{patch}");
        assert_eq!(r["facts"]["written"], false);
        assert_eq!(f.writes().len(), before, "{patch}: no keys sent");
        assert_eq!(
            f.tui()["input"],
            patch.get("input").cloned().unwrap_or(json!(""))
        );
    }
    assert!(
        !records(&transcript)
            .iter()
            .any(|x| x.to_string().contains("should not arrive"))
    );

    // 5. Submitted but never recorded: uncertain, never re-sent.
    reset(json!({"record":"none"}));
    let mut b = body("submit", &f.session(&id), "lost one.");
    b["timeout_seconds"] = json!(2);
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "delivery_unknown", "{r}");
    assert_eq!(r["reason"], "TERMINAL_INPUT_UNCONFIRMED");
    assert_eq!(r["execution"]["automatic_replay"], false);

    // 6. Model steering on: the running session keeps its old value until restarted.
    let on = f.steering("--enable");
    assert_eq!(on["restart_required"], true);
    reset(json!({}));
    let s = f.session(&id);
    assert_eq!(
        s["input_delivery"]["model_steering"]["restart_required"], true,
        "{s}"
    );
    assert!(
        s["capability_reasons"]["steer"]
            .as_str()
            .unwrap()
            .starts_with("GEMINI_STEERING_RESTART_REQUIRED")
    );
    assert!(
        f.send_err(&body("steer", &s, "x"))
            .contains("GEMINI_STEERING_RESTART_REQUIRED")
    );
    drop(process);
    std::thread::sleep(Duration::from_millis(1100));
    let process = f.gemini("--resume", &native);
    let base = json!({"pane":"w1:p1","pid":process.0.id(),"transcript":transcript,"steering":true});
    let reset = |patch: Value| {
        let mut v = base.clone();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        f.set_tui(&v);
    };
    reset(json!({}));
    let mut s = Value::Null;
    wait_for("restarted session", || {
        s = f.session(&id);
        s["pid"] == process.0.id() && s["attachment"]["host"] == "herdr"
    });
    assert_eq!(
        s["input_delivery"]["modes"]["steer"], "terminal_input",
        "{s}"
    );
    assert_eq!(s["input_delivery"]["steer"]["label"], "Steer");
    assert!(s["input_delivery"]["steer"]["caveat"].is_string());
    assert!(
        s["capability_reasons"]["queue"]
            .as_str()
            .unwrap()
            .starts_with("GEMINI_QUEUE_WOULD_STEER")
    );

    // 7. Steer applied at the next tool step.
    reset(json!({"status":"working"}));
    let b = body("steer", &s, "also   check the docs.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["correlation"]["record_type"], "steering_update");
    assert_eq!(r["gemini_input"]["outcome"], "steering_applied");
    assert_eq!(r["gemini_input"]["steering"]["applied"], true);
    assert_eq!(r["gemini_input"]["screen_evidence"], "steering_hint");
    assert_eq!(r["reply"]["text"], "adjusted for: also check the docs.");
    assert_eq!(r["facts"]["folded"], true);

    // 8. Steer not applied: the turn ends without a tool call, so Gemini submits
    //    it afterwards as its own steering prompt.
    reset(json!({"status":"working","tool_follows":false}));
    let b = body("steer", &f.session(&id), "prefer the small fix.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["gemini_input"]["steering"]["applied"], false);
    assert_eq!(
        r["gemini_input"]["steering"]["fate"],
        "submitted_after_turn_as_steering_prompt"
    );

    // 9. A busy submit would become a steering update: refused before Enter,
    //    the typed text withdrawn and nothing submitted.
    reset(json!({"status":"working","input":"my draft"}));
    let b = body("submit", &f.session(&id), "plain submit.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "rejected", "{r}");
    assert_eq!(r["reason"], "GEMINI_SUBMIT_WOULD_STEER");
    assert_eq!(f.tui()["input"], "my draft");
    assert!(
        !records(&transcript)
            .iter()
            .any(|x| x.to_string().contains("plain submit"))
    );
    assert!(!f.herdr.join("forbidden").exists());
    drop(process);
}
