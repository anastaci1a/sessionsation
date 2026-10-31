//! Claude terminal input (contract revision 17) against fakes only:
//! `support/fake-herdr-tui.py` (HERDR with one pane showing a fake Claude
//! prompt) and `support/fake-claude-peer.py` (the live registered process the
//! pane runs). Disposable homes; no real pane, provider or model is touched.
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
    tmp: tempfile::TempDir,
    paths: Paths,
    project: PathBuf,
    bin: PathBuf,
    herdr: PathBuf,
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
    let claude = install(
        tmp.path().join("fake-claude"),
        include_str!("support/fake-claude.sh"),
    );
    install(
        tmp.path().join("fake-claude-peer.py"),
        include_str!("support/fake-claude-peer.py"),
    );
    install(bin.join("herdr"), include_str!("support/fake-herdr-tui.py"));
    let codex = install(
        tmp.path().join("fake-codex"),
        include_str!("support/fake-codex.sh"),
    );
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
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
        tmp,
        paths,
        project,
        bin,
        herdr,
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
            .env_remove("CLAUDE_CONFIG_DIR")
            .env(
                "PATH",
                format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("FAKE_HERDR_DIR", &self.herdr)
            .env("ANTHROPIC_API_KEY", "sk-test-must-not-reach-claude")
            .stdin(Stdio::null());
        c
    }
    fn api(&self, args: &[&str]) -> Value {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.command(&all).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    }
    fn home(&self) -> PathBuf {
        self.paths.root.join("accounts/claude").join("personal")
    }
    fn session(&self, id: &str) -> Value {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id || s["conversation_id"] == id)
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
    /// HERDR writes (typed text or keys) recorded by the fake so far.
    fn writes(&self) -> Vec<Vec<String>> {
        read(&self.herdr.join("calls"))
            .lines()
            .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
            .filter(|a| a.iter().any(|x| x == "send-text" || x == "send-keys"))
            .collect()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_input_types_as_the_user_and_keeps_the_draft() {
    let f = fixture().await;
    let out = f
        .command(&["account", "add", "--platform", "claude", "personal"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for("auth status poll", || {
        f.api(&["account", "list"])[0]["auth_required"] == false
    });
    let home = f.home();
    let conversation = uuid::Uuid::new_v4().to_string();
    let socket = f.tmp.path().join("p.sock");
    // The interactive Claude process running in HERDR pane w1:p1.
    let claude = Process(
        Command::new("python3")
            .arg(f.tmp.path().join("fake-claude-peer.py"))
            .args([
                &conversation,
                f.project.to_str().unwrap(),
                socket.to_str().unwrap(),
            ])
            .env("CLAUDE_CONFIG_DIR", &home)
            .env("HERDR_PANE_ID", "w1:p1")
            .env("HERDR_SESSION", "fixture")
            .env("HERDR_ENV", "1")
            .current_dir(&f.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let transcript = home
        .join("projects")
        .join(f.project.to_str().unwrap().replace('/', "-"))
        .join(format!("{conversation}.jsonl"));
    let screens = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-screens");
    let base = json!({"pane":"w1:p1","pid":claude.0.id(),"view":"main","input":"","status":"idle",
        "transcript":transcript,"screens":screens});
    let reset = |patch: Value| {
        let mut v = base.clone();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        f.set_tui(&v);
    };
    reset(json!({}));
    let mut s = Value::Null;
    wait_for("attached live session", || {
        s = f.session(&conversation);
        s["availability"] == "running" && s["attachment"]["host"] == "herdr"
    });
    let id = s["id"].as_str().unwrap().to_owned();
    // Only terminal input applies (live steering is off): submit/steer typed,
    // queue refused with its own reason.
    assert_eq!(s["input_delivery"]["semantics"], "terminal_input", "{s}");
    assert_eq!(s["input_delivery"]["terminal_input"]["available"], true);
    assert_eq!(s["input_delivery"]["modes"]["queue"], Value::Null);
    assert_eq!(s["capabilities"]["steer"], true);
    assert_eq!(s["capabilities"]["queue"], false);
    assert!(
        s["capability_reasons"]["queue"]
            .as_str()
            .unwrap()
            .starts_with("TERMINAL_QUEUE_UNSUPPORTED")
    );
    assert!(
        f.send_err(&body("queue", &s, "later"))
            .contains("TERMINAL_QUEUE_UNSUPPORTED")
    );
    let mut interrupt = body("steer", &s, "x");
    interrupt["steering"] = json!({"version":1,"interrupt":true});
    assert!(
        f.send_err(&interrupt)
            .contains("TERMINAL_INTERRUPT_UNSUPPORTED")
    );
    assert!(
        f.send_err(&body("steer", &s, "see @src/main.rs"))
            .contains("TERMINAL_PAYLOAD_UNSUPPORTED")
    );
    assert!(f.writes().is_empty(), "admission refusals write nothing");

    // 1. Idle session, a two-line draft: cleared, message typed and submitted,
    //    draft typed back unsubmitted; the reply anchors to the user record.
    let draft = "half a thought\nsecond line";
    reset(json!({"input":draft}));
    let steer = body("steer", &f.session(&id), "please check\nthe build.");
    let m = steer["message_id"].as_str().unwrap().to_owned();
    let first = f.send_ok(&steer);
    assert_eq!(first["delivery_semantics"], "terminal_input");
    let r = f.settled(&m);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(
        r["reply"]["text"],
        "terminal reply to: please check\nthe build."
    );
    let user = records(&transcript)
        .into_iter()
        .find(|x| x["message"]["content"] == "please check\nthe build.")
        .unwrap();
    assert_eq!(r["correlation"]["input_uuid"], user["uuid"]);
    assert_eq!(r["correlation"]["record_type"], "user");
    assert_eq!(r["terminal_input"]["draft"]["restored"], true);
    assert_eq!(r["terminal_input"]["draft"]["chars"], draft.chars().count());
    assert_eq!(r["terminal_input"]["draft"]["left_in_input"], "draft");
    assert_eq!(
        f.tui()["input"],
        draft,
        "draft back in the prompt, unsubmitted"
    );
    // The Enter started the turn: the draft was typed back on the real busy
    // screen (grey pointer), which the rev 17 cutover refused as a subagent view.
    assert_eq!(
        read(&f.herdr.join("typed-status")).lines().last(),
        Some("working"),
        "restored while the turn runs"
    );
    assert!(
        !r.to_string().contains("half a thought"),
        "the draft never reaches receipts"
    );
    let journal: Value = serde_json::from_str(&read(
        &f.paths
            .root
            .join(sessionsation::layout::CLAUDE_STEER)
            .join(format!("{m}.json")),
    ))
    .unwrap();
    assert!(
        journal["terminal"]["draft"].is_null(),
        "restored draft dropped from the journal"
    );
    let writes = f.writes();
    let flat: Vec<&String> = writes.iter().flatten().collect();
    assert!(
        flat.iter().all(|x| !x.contains("\u{1b}[200~")),
        "typed, never pasted"
    );
    assert_eq!(flat.iter().filter(|x| x.as_str() == "enter").count(), 1);
    assert!(
        flat.iter().any(|x| x.as_str() == "ctrl+j"),
        "newline typed with ctrl+j"
    );
    assert!(
        !f.herdr.join("forbidden").exists(),
        "no Esc/Ctrl+C/Ctrl+D/Ctrl+L"
    );
    // Same id and payload: the original receipt, nothing re-sent.
    assert_eq!(f.send_ok(&steer)["state"], "completed_with_reply");
    assert_eq!(f.writes().len(), writes.len());

    // 2. Mid-turn: Claude queues the typed message as a human queued_command.
    reset(json!({"status":"working"}));
    let b = body("steer", &f.session(&id), "also run clippy.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["correlation"]["record_type"], "queued_command");
    assert_eq!(r["facts"]["folded"], true);
    assert_eq!(r["terminal_input"]["draft"]["present"], false);
    let queued = |text: &str| {
        records(&transcript).into_iter().find(|x| {
            x["attachment"]["type"] == "queued_command" && x["attachment"]["prompt"] == text
        })
    };
    let q = queued("also run clippy.").unwrap();
    assert_eq!(q["attachment"]["origin"]["kind"], "human");
    assert_eq!(r["correlation"]["input_uuid"], q["uuid"]);

    // 2b. Mid-turn with a draft in the box (busy screen, grey pointer): the
    //     steer is queued and the draft typed back while the turn still runs.
    let busy_draft = "keep this\nbusy draft";
    reset(json!({"status":"working","working_polls":12,"input":busy_draft}));
    let b = body("steer", &f.session(&id), "steer with a draft.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["correlation"]["record_type"], "queued_command");
    let q = queued("steer with a draft.").unwrap();
    assert_eq!(q["attachment"]["origin"]["kind"], "human");
    assert_eq!(r["correlation"]["input_uuid"], q["uuid"]);
    assert_eq!(r["terminal_input"]["draft"]["restored"], true, "{r}");
    assert_eq!(f.tui()["input"], busy_draft);
    assert!(
        read(&f.herdr.join("typed-status"))
            .lines()
            .rev()
            .take(2)
            .all(|s| s == "working")
    );

    // 3. A leading slash is escaped: plain text, not a command.
    reset(json!({}));
    let b = body("submit", &f.session(&id), "/compact the notes.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["terminal_input"]["escaped_prefix"], true);
    assert!(
        records(&transcript)
            .iter()
            .any(|x| x["message"]["content"] == "\\/compact the notes.")
    );

    // 4. Views left with their own toggles: transcript (ctrl+o), help (`?`).
    for (view, key) in [("transcript", "ctrl+o"), ("help", "?")] {
        reset(json!({"view":view}));
        let b = body("steer", &f.session(&id), &format!("from {view}."));
        f.send_ok(&b);
        let r = f.settled(b["message_id"].as_str().unwrap());
        assert_eq!(r["state"], "completed_with_reply", "{view}: {r}");
        assert_eq!(r["terminal_input"]["view_switched"], view);
        assert!(f.writes().iter().flatten().any(|x| x == key));
    }

    // 5. Refusals before any write: nothing typed, the draft untouched.
    for (patch, code) in [
        (
            json!({"input":"[Pasted text #1 +40 lines]"}),
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        (
            json!({"status":"blocked","input":"keep me"}),
            "TERMINAL_AGENT_BLOCKED",
        ),
        (
            json!({"view":"subagent","input":"to the worker"}),
            "TERMINAL_VIEW_NOT_MAIN",
        ),
        (
            json!({"view":"menu","input":"/mo"}),
            "TERMINAL_OVERLAY_UNIDENTIFIED",
        ),
        (json!({"view":"subagent"}), "TERMINAL_VIEW_NOT_MAIN"),
        (json!({"view":"agents"}), "TERMINAL_OVERLAY_UNIDENTIFIED"),
        (json!({"view":"blank"}), "TERMINAL_VIEW_UNDETERMINED"),
        (json!({"vim":true,"input":"x"}), "TERMINAL_VIM_MODE"),
    ] {
        reset(patch.clone());
        let before = f.writes().len();
        let b = body("steer", &f.session(&id), "should not arrive.");
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

    // 6. Submitted but never recorded: uncertain, never re-sent.
    reset(json!({"record":"none"}));
    let mut b = body("steer", &f.session(&id), "lost one.");
    b["timeout_seconds"] = json!(2);
    let journal_draft = |m: &str| {
        let j: Value = serde_json::from_str(&read(
            &f.paths
                .root
                .join(sessionsation::layout::CLAUDE_STEER)
                .join(format!("{m}.json")),
        ))
        .unwrap();
        j["terminal"]["draft"].clone()
    };
    let enters = |f: &Fixture| {
        f.writes()
            .iter()
            .flatten()
            .filter(|x| *x == "enter")
            .count()
    };
    let before = enters(&f);
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "delivery_unknown", "{r}");
    assert_eq!(r["reason"], "TERMINAL_INPUT_UNCONFIRMED");
    assert!(r["reply"].is_null());
    assert_eq!(r["execution"]["automatic_replay"], false);
    assert_eq!(enters(&f), before + 1);

    // 7. A restore that does not verify is reported, not retried.
    reset(json!({"input":"my draft","drop_after_submit":true}));
    let b = body("steer", &f.session(&id), "restore check.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["terminal_input"]["draft"]["restored"], false);
    assert_eq!(
        r["terminal_input"]["draft"]["restore_error"],
        "restore_unverified"
    );
    // Exactly what the box holds is reported; the draft stays in the journal.
    assert_eq!(r["terminal_input"]["draft"]["left_in_input"], "empty");
    assert_eq!(r["terminal_input"]["draft"]["left_chars"], 0);
    assert_eq!(journal_draft(b["message_id"].as_str().unwrap()), "my draft");

    // 7b. The typed text does not match: withdrawn, the draft typed back, no Enter.
    reset(json!({"input":"garbled draft","garble_once":true}));
    let before = enters(&f);
    let b = body("steer", &f.session(&id), "will be garbled.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "rejected", "{r}");
    assert_eq!(r["reason"], "TERMINAL_TYPED_TEXT_MISMATCH");
    assert_eq!(r["terminal_input"]["draft"]["restored"], true);
    assert_eq!(f.tui()["input"], "garbled draft");
    assert_eq!(enters(&f), before);

    // 7c. Enter itself fails after the clear: uncertain, and the box is
    //     reported exactly (the unsent message), the draft kept privately.
    reset(json!({"input":"enter draft","fail_enter":true}));
    let b = body("steer", &f.session(&id), "enter fails.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "delivery_unknown", "{r}");
    assert_eq!(r["reason"], "TERMINAL_SUBMIT_UNCERTAIN");
    assert_eq!(r["terminal_input"]["draft"]["restored"], false);
    assert_eq!(r["terminal_input"]["draft"]["left_in_input"], "message");
    assert!(
        r["terminal_input"]["draft"]["restore_error"]
            .as_str()
            .unwrap()
            .starts_with("host_failed")
    );
    assert_eq!(f.tui()["input"], "enter fails.");
    assert_eq!(
        journal_draft(b["message_id"].as_str().unwrap()),
        "enter draft"
    );

    // 7d. A several-KB, multi-paragraph message in a narrow, short box (it
    //     shows only the tail): typed in segments, each verified on screen.
    let long = (0..14)
        .map(|i| {
            format!(
                "Paragraph {i}: {}",
                "word-and-/a/very/long/path_token ".repeat(8)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    assert!(long.len() > 3500);
    reset(json!({"input":"","wrap":30,"window":12}));
    let b = body("steer", &f.session(&id), &long);
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");

    // 7e. A corrupted early part of a scrolled message is still caught.
    reset(json!({"input":"","wrap":30,"window":12,"garble_once":true}));
    let b = body("steer", &f.session(&id), &long);
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "rejected", "{r}");
    assert_eq!(r["reason"], "TERMINAL_TYPED_TEXT_MISMATCH");

    // 7f. Text over the limit is refused up front.
    let over = "x ".repeat(4200);
    assert!(
        f.send_err(&body("steer", &f.session(&id), &over))
            .contains("TERMINAL_TEXT_TOO_LONG")
    );

    // 8. With live steering on, typed input still wins for submit/steer; queue
    //    and an explicit peer_message use the peer path.
    let out = f
        .command(&["account", "live-steering", "claude:personal", "--enable"])
        .output()
        .unwrap();
    assert!(out.status.success());
    reset(json!({}));
    let s = f.session(&id);
    assert_eq!(s["input_delivery"]["semantics"], "terminal_input");
    assert_eq!(s["input_delivery"]["modes"]["queue"], "peer_message");
    assert_eq!(
        s["input_delivery"]["available"],
        json!(["terminal_input", "peer_message"])
    );
    let q = f.send_ok(&body("queue", &s, "queued via peer."));
    assert_eq!(q["delivery_semantics"], "peer_message");
    let mut p = body("steer", &s, "explicit peer.");
    p["delivery"] = json!("peer_message");
    assert_eq!(f.send_ok(&p)["delivery_semantics"], "peer_message");
    // 9. The operator kill switch: rows report terminal input unavailable and
    //    nothing is typed.
    let mut config = sessionsation::store::load_config(&f.paths).unwrap();
    config.terminal_input_enabled = false;
    sessionsation::store::save_config(&f.paths, &config).unwrap();
    let s = f.session(&id);
    assert_eq!(s["input_delivery"]["terminal_input"]["available"], false);
    assert_eq!(
        s["input_delivery"]["terminal_input"]["reason"],
        "TERMINAL_INPUT_DISABLED"
    );
    assert_eq!(s["input_delivery"]["semantics"], "peer_message");
    let before = f.writes().len();
    let mut t = body("steer", &s, "while disabled.");
    t["delivery"] = json!("terminal_input");
    assert!(f.send_err(&t).contains("TERMINAL_INPUT_DISABLED"));
    assert_eq!(f.writes().len(), before);
    config.terminal_input_enabled = true;
    sessionsation::store::save_config(&f.paths, &config).unwrap();
    assert_eq!(
        f.session(&id)["input_delivery"]["terminal_input"]["available"],
        true
    );
    drop(claude);
}
