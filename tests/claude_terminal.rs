//! Claude terminal input (contract revision 17) against fakes only:
//! `support/fake-herdr-tui.py` (HERDR with one pane showing a fake Claude
//! prompt) and `support/fake-claude-peer.py` (the live registered process the
//! pane runs). Disposable homes; no real pane, provider or model is touched.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
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
    fixture_with(false).await
}
/// `daemon_herdr`: the supervisor also runs with the fake HERDR on its PATH (it starts the
/// delivery workers it re-dispatches with its own environment).
async fn fixture_with(daemon_herdr: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let herdr = tmp.path().join("herdr-state");
    std::fs::create_dir(&herdr).unwrap();
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
            .envs(
                daemon_herdr
                    .then(|| {
                        [
                            ("FAKE_HERDR_DIR".to_owned(), herdr.display().to_string()),
                            (
                                "PATH".to_owned(),
                                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
                            ),
                        ]
                    })
                    .into_iter()
                    .flatten(),
            )
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
        "expected_generation":session["generation"],"text":text,"expires_at":daemon::now()+600,"await_reply":true})
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
        // Contract 1.63.0: the provider exited and the pane's shell is in front.
        (
            json!({"shell_foreground":true,"input":"keep me"}),
            "TERMINAL_PANE_IS_SHELL",
        ),
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

/// Contract 1.40.0: agent-originated sends try the native peer message first and
/// type into the pane only when nothing native was possible; operator sends keep
/// typing; one message is never delivered twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_sends_try_native_first_and_fall_back_to_typing_once() {
    let f = fixture().await;
    let out = f
        .command(&["account", "add", "--platform", "claude", "personal"])
        .output()
        .unwrap();
    assert!(out.status.success());
    wait_for("auth status poll", || {
        f.api(&["account", "list"])[0]["auth_required"] == false
    });
    let home = f.home();
    let conversation = uuid::Uuid::new_v4().to_string();
    let socket = f.tmp.path().join("p.sock");
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
    f.set_tui(
        &json!({"pane":"w1:p1","pid":claude.0.id(),"view":"main","input":"","status":"idle",
        "transcript":transcript,"screens":screens}),
    );
    let mut s = Value::Null;
    wait_for("attached live session", || {
        s = f.session(&conversation);
        s["availability"] == "running" && s["attachment"]["host"] == "herdr"
    });
    let id = s["id"].as_str().unwrap().to_owned();
    let frames = || {
        read(&home.join("fake-peer-frames.jsonl"))
            .lines()
            .filter(|l| l.contains("\"type\": \"user\""))
            .count()
    };
    let agent = |text: &str, mode: &str| {
        let mut b = body(mode, &f.session(&id), text);
        b["origin"] = json!("agent");
        b
    };

    // 1. The session does not accept peer messages: typed instead, with the reason.
    let b = agent("typed because native is off", "steer");
    let m = b["message_id"].as_str().unwrap().to_owned();
    let first = f.send_ok(&b);
    assert_eq!(first["delivery_semantics"], "terminal_input", "{first}");
    assert_eq!(first["native_fallback"]["from"], "peer_message");
    assert_eq!(first["native_fallback"]["reason"], "LIVE_STEERING_DISABLED");
    assert_eq!(first["native_fallback"]["stage"], "admission");
    let r = f.settled(&m);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(frames(), 0, "nothing was sent natively");
    let typed = f.writes().len();
    assert!(typed > 0);

    // 2. Queue cannot be typed: native only, so it is refused rather than typed.
    let err = f.send_err(&agent("later", "queue"));
    assert!(err.contains("LIVE_STEERING_DISABLED"), "{err}");
    assert_eq!(f.writes().len(), typed);

    // 3. Accepting: the agent send goes native and nothing is typed; operator keeps typing.
    let toggled = f.api(&["account", "live-steering", "claude:personal", "--enable"]);
    assert_eq!(toggled["live_steering"], true);
    let b = agent("native now", "steer");
    let m = b["message_id"].as_str().unwrap().to_owned();
    let first = f.send_ok(&b);
    assert_eq!(first["delivery_semantics"], "peer_message", "{first}");
    assert_eq!(first["native_fallback"], Value::Null);
    let r = f.settled(&m);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(frames(), 1);
    assert_eq!(f.writes().len(), typed, "nothing typed for a native send");
    let mut operator = body("steer", &f.session(&id), "typed for the operator");
    operator["delivery"] = json!("terminal_input");
    operator["origin"] = json!("operator");
    let first = f.send_ok(&operator);
    assert_eq!(first["delivery_semantics"], "terminal_input");
    f.settled(operator["message_id"].as_str().unwrap());
    assert_eq!(frames(), 1, "the operator send was not sent natively too");

    // 4. The native endpoint disappears (socket file removed): typed, once.
    std::fs::remove_file(&socket).unwrap();
    let b = agent("endpoint gone", "steer");
    let first = f.send_ok(&b);
    assert_eq!(first["delivery_semantics"], "terminal_input", "{first}");
    assert_eq!(first["native_fallback"]["reason"], "PEER_SOCKET_UNLINKED");
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(frames(), 1);
    assert_eq!(f.send_ok(&b)["state"], "completed_with_reply");
    assert_eq!(
        frames(),
        1,
        "same id: the original receipt, nothing re-sent"
    );
}

/// A live fake Claude in pane w1:p1 with an idle prompt; returns the process, the session id and
/// a function that resets the fake pane's state.
async fn live_pane(f: &Fixture) -> (Process, String, Box<dyn Fn(Value) + '_>) {
    let out = f
        .command(&["account", "add", "--platform", "claude", "personal"])
        .output()
        .unwrap();
    assert!(out.status.success());
    wait_for("auth status poll", || {
        f.api(&["account", "list"])[0]["auth_required"] == false
    });
    let home = f.home();
    let conversation = uuid::Uuid::new_v4().to_string();
    let socket = f.tmp.path().join("p.sock");
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
    let reset = move |patch: Value| {
        let mut v = base.clone();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        std::fs::write(f.herdr.join("tui.json"), v.to_string()).unwrap();
    };
    reset(json!({}));
    let mut s = Value::Null;
    wait_for("attached live session", || {
        s = f.session(&conversation);
        s["availability"] == "running" && s["attachment"]["host"] == "herdr"
    });
    let id = s["id"].as_str().unwrap().to_owned();
    (claude, id, Box::new(reset))
}

/// Put the journal files back after the fake broke them (see `break_journal`).
fn mend_journal(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") && p.is_dir() {
            std::fs::remove_dir(&p).unwrap();
            let held = PathBuf::from(format!("{}.held", p.display()));
            std::fs::rename(held, &p).unwrap();
        }
    }
}

/// Contract 1.41.0: a worker that fails between typing and Enter (a journal write that fails on a
/// full disk) withdraws the message it typed, even when the user had no draft. Nothing is sent, the
/// request is never reported delivered, and the box is empty again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_between_typing_and_enter_withdraws_the_typed_message() {
    let f = fixture().await;
    let (_claude, id, reset) = live_pane(&f).await;
    let journal = f.paths.root.join("messaging/claude-steer");
    reset(json!({"input":"","break_journal":journal,"break_at_chunk":5}));
    let enters_before = f
        .writes()
        .iter()
        .filter(|w| w.iter().any(|x| x == "enter"))
        .count();
    let b = body(
        "submit",
        &f.session(&id),
        &"this must never be left unsent. ".repeat(8),
    );
    let m = b["message_id"].as_str().unwrap().to_owned();
    f.send_ok(&b);
    wait_for("the journal to break", || f.tui()["journal_broken"] == true);
    // The worker fails at its next journal write; give it the time to withdraw.
    wait_for("the typed text to be withdrawn", || f.tui()["input"] == "");
    wait_for("the worker to finish", || {
        std::fs::read_dir(&journal).unwrap().flatten().all(|e| {
            let p = e.path();
            p.extension().is_none_or(|x| x != "lock") || sessionsation_worker_stopped(&p)
        })
    });
    assert_eq!(
        f.writes()
            .iter()
            .filter(|w| w.iter().any(|x| x == "enter"))
            .count(),
        enters_before,
        "Enter was never sent"
    );
    mend_journal(&journal);
    // Unsent: never reported as delivered; settled as rejected once the dead worker is certain.
    let r = f.settled(&m);
    assert_eq!(r["state"], "rejected", "{r}");
    assert_eq!(r["reason"], "TERMINAL_WORKER_EXITED_BEFORE_SUBMIT");
    assert_eq!(r["facts"]["input_recorded"], false);
    assert_eq!(f.tui()["input"], "");
}

fn sessionsation_worker_stopped(lock: &Path) -> bool {
    use fs2::FileExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock)
        .map(|f| f.try_lock_exclusive().is_ok())
        .unwrap_or(true)
}

/// Contract 1.41.0: a worker that died with the typed message still in the box (and the user's
/// draft taken out) is cleaned up: by a cleanup worker when its receipt is read, or before the next
/// delivery on that pane, which then sees the user's own draft instead of an unreconstructable one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_orphaned_typed_message_is_withdrawn_and_the_draft_restored() {
    let f = fixture().await;
    let (_claude, id, reset) = live_pane(&f).await;
    let journal = f.paths.root.join("messaging/claude-steer");
    // The journal and the screen both fail after the first typed chunk: the worker can neither
    // record nor clean anything.
    reset(
        json!({"input":"my draft","break_journal":journal,"break_screen_after_type":true,"break_at_chunk":5}),
    );
    let msg = "left behind by a dead worker. ".repeat(8);
    let b = body("submit", &f.session(&id), &msg);
    let m = b["message_id"].as_str().unwrap().to_owned();
    f.send_ok(&b);
    wait_for("the worker to give up", || {
        let t = f.tui();
        t["screen_broken"] == true
            && std::fs::read_dir(&journal).unwrap().flatten().all(|e| {
                let p = e.path();
                p.extension().is_none_or(|x| x != "lock") || sessionsation_worker_stopped(&p)
            })
    });
    let t = f.tui();
    let stuck = t["input"].as_str().unwrap();
    assert!(
        stuck.len() > 32 && msg.starts_with(stuck),
        "the typed part of the message is stuck in the input box: {stuck:?}"
    );
    mend_journal(&journal);
    // The pane is readable again; a receipt read starts the cleanup.
    let mut v = f.tui();
    v["screen_broken"] = json!(false);
    v.as_object_mut().unwrap().remove("break_journal");
    v.as_object_mut().unwrap().remove("break_screen_after_type");
    f.set_tui(&v);
    let r = f.settled(&m);
    assert_eq!(r["state"], "rejected", "{r}");
    assert_eq!(r["reason"], "TERMINAL_WORKER_EXITED_BEFORE_SUBMIT");
    assert_eq!(r["facts"]["input_recorded"], false);
    assert_eq!(r["terminal_input"]["draft"]["restored"], true, "{r}");
    assert_eq!(f.tui()["input"], "my draft", "the user's draft is back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_next_delivery_withdraws_an_orphan_instead_of_refusing_its_text() {
    let f = fixture().await;
    let (_claude, id, reset) = live_pane(&f).await;
    let journal = f.paths.root.join("messaging/claude-steer");
    reset(
        json!({"input":"","break_journal":journal,"break_screen_after_type":true,"break_at_chunk":5}),
    );
    let msg = "orphan text that stays in the box. ".repeat(8);
    let b = body("submit", &f.session(&id), &msg);
    let orphan = b["message_id"].as_str().unwrap().to_owned();
    f.send_ok(&b);
    wait_for("the worker to give up", || {
        f.tui()["screen_broken"] == true
            && std::fs::read_dir(&journal).unwrap().flatten().all(|e| {
                let p = e.path();
                p.extension().is_none_or(|x| x != "lock") || sessionsation_worker_stopped(&p)
            })
    });
    let stuck = f.tui()["input"].as_str().unwrap().to_owned();
    assert!(stuck.len() > 32 && msg.starts_with(&stuck), "{stuck:?}");
    mend_journal(&journal);
    let mut v = f.tui();
    v["screen_broken"] = json!(false);
    v.as_object_mut().unwrap().remove("break_journal");
    v.as_object_mut().unwrap().remove("break_screen_after_type");
    f.set_tui(&v);
    // A new send to the same pane: the orphan's text is withdrawn first, then this one is delivered.
    let next = body("submit", &f.session(&id), "the next message.");
    let n = next["message_id"].as_str().unwrap().to_owned();
    f.send_ok(&next);
    let r = f.settled(&n);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["reply"]["text"], "terminal reply to: the next message.");
    let o = f.api(&["receipt", &orphan]);
    assert_eq!(o["state"], "rejected", "{o}");
    assert_eq!(o["facts"]["input_recorded"], false);
}

/// A refused screen records the layout around the input box (never the draft), so a refusal can be
/// diagnosed from its receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_screen_reports_its_layout() {
    let f = fixture().await;
    let (_claude, id, reset) = live_pane(&f).await;
    // A long unsent draft in a narrow, short window: more rows than a draft may take.
    let long = (0..14)
        .map(|i| format!("line {i} of someone's unsent draft"))
        .collect::<Vec<_>>()
        .join("\n");
    reset(json!({"input":long,"wrap":0,"window":20}));
    let b = body("submit", &f.session(&id), "will be refused.");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "rejected", "{r}");
    assert_eq!(r["reason"], "TERMINAL_DRAFT_UNRECONSTRUCTABLE", "{r}");
    let layout = &r["terminal_input"]["refusal_layout"];
    assert!(layout["box_rows"].as_u64().unwrap() > 12, "{layout}");
    assert_eq!(layout["max_draft_rows"], 12);
    assert!(!layout.to_string().contains("someone's unsent draft"));
    assert_eq!(f.tui()["input"], long, "the user's draft was not touched");
}

#[path = "support/parity_keys.rs"]
mod parity_keys;

/// Contract 1.64.0 (SM1): the one `api send` shape reaches a live Claude session as typed
/// input. Codex-only options (`if_busy`, a recovery intent, a Codex turn target) are named in
/// `ignored_options`, never refused; mid-turn, `deliver` is Claude's own `queued_command`
/// and the receipt says `delivery.queued` until the running turn reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_send_types_into_claude_ignores_codex_options_and_queues_when_busy() {
    let f = fixture().await;
    let (_claude, id, reset) = live_pane(&f).await;
    let s = f.session(&id);
    let body = parity_keys::unified_send(&s, "deliver", "hello claude", daemon::now() + 600);
    let sent = f.send_ok(&body);
    parity_keys::assert_delivery(&sent, "deliver", "submit", "terminal_input", &["if_busy"]);
    assert!(
        sent["ignored_reason"]
            .as_str()
            .unwrap()
            .starts_with("Claude sessions do not use if_busy"),
        "{sent}"
    );
    let done = f.settled(body["message_id"].as_str().unwrap());
    assert_eq!(done["state"], "completed_with_reply", "{done}");
    assert_eq!(done["reply"]["text"], "terminal reply to: hello claude");
    parity_keys::assert_delivery(&done, "deliver", "submit", "terminal_input", &["if_busy"]);
    // Codex options that used to be refused for Claude.
    reset(json!({}));
    let mut body = parity_keys::unified_send(&s, "interrupt", "steer this", daemon::now() + 600);
    body["recovery"] = json!({"version":1,"expected_stop_id":"stop-1"});
    body["steering"] = json!({"version":1,"expected_turn_id":"turn-1","expected_runtime_generation":"a".repeat(64)});
    let sent = f.send_ok(&body);
    let ignored = [
        "if_busy",
        "recovery",
        "steering.expected_turn_id",
        "steering.expected_runtime_generation",
    ];
    parity_keys::assert_delivery(&sent, "interrupt", "steer", "terminal_input", &ignored);
    let done = f.settled(body["message_id"].as_str().unwrap());
    assert_eq!(done["state"], "completed_with_reply", "{done}");
    parity_keys::assert_delivery(&done, "interrupt", "steer", "terminal_input", &ignored);
    // Mid-turn: Claude queues the typed text until the running turn reaches it.
    reset(json!({"status":"working","working_polls":100000,"hold_queued":true}));
    let body = parity_keys::unified_send(&s, "deliver", "after this turn", daemon::now() + 600);
    let id = body["message_id"].as_str().unwrap().to_owned();
    let sent = f.send_ok(&body);
    parity_keys::assert_delivery(&sent, "deliver", "submit", "terminal_input", &["if_busy"]);
    let mut r = Value::Null;
    wait_for("queued behind the turn", || {
        r = f.api(&["receipt", &id]);
        r["delivery"]["queued"] == true
    });
    assert_eq!(r["state"], "native_accepted", "{r}");
    assert_eq!(r["correlation"]["record_type"], "queued_command", "{r}");
    assert_eq!(r["delivery"]["held"], false);
    std::fs::write(f.herdr.join("end-turn"), "").unwrap();
    let done = f.settled(&id);
    assert_eq!(done["state"], "completed_with_reply", "{done}");
    assert_eq!(done["delivery"]["queued"], false, "{done}");
    assert!(
        read(&f.herdr.join("forbidden")).is_empty(),
        "no forbidden key"
    );
}

/// The exact delivery worker process of `message` (its full argument list), if one runs.
fn worker_pid(f: &Fixture, message: &str) -> Option<u32> {
    let root = f.paths.root.to_str().unwrap();
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: u32 = e.file_name().to_str()?.parse().ok()?;
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
        let tail: [&[u8]; 4] = [
            b"--data-dir",
            root.as_bytes(),
            b"claude-terminal-worker",
            message.as_bytes(),
        ];
        (args.len() >= 5 && args[1..5] == tail && !zombie(pid)).then_some(pid)
    })
}
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(
        stat.rsplit_once(") ")?
            .1
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    )
}
fn zombie(pid: u32) -> bool {
    stat_fields(pid).is_none_or(|s| s[0] == "Z" || s[0] == "X")
}
/// SIGKILL every process of session `sid` (exact session id, read from /proc), and its
/// process group.
fn kill_session(sid: i32) {
    unsafe {
        libc::kill(-sid, libc::SIGKILL);
    }
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if stat_fields(pid)
            .is_some_and(|s| s.get(3).and_then(|x| x.parse::<i32>().ok()) == Some(sid))
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

/// Contract 1.64.0: the delivery worker belongs to the supervisor. The sender runs in its
/// own session and that whole session is killed right after the send (as when a retired
/// agent's environment is torn down); the message is still delivered, by its first worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_is_delivered_after_the_senders_session_is_killed() {
    use std::os::unix::process::CommandExt;
    let f = fixture_with(true).await;
    let (_claude, id, _reset) = live_pane(&f).await;
    let s = f.session(&id);
    std::fs::write(f.herdr.join("hold-read"), "").unwrap();
    let b = body("submit", &s, "survives the sender");
    let message = b["message_id"].as_str().unwrap().to_owned();
    let mut sender = f.command(&["api", "send"]);
    sender
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        sender.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = sender.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b.to_string().as_bytes())
        .unwrap();
    let sid = child.id() as i32;
    wait_for("delivery worker", || worker_pid(&f, &message).is_some());
    kill_session(sid);
    let _ = child.wait();
    std::fs::remove_file(f.herdr.join("hold-read")).unwrap();
    let done = f.settled(&message);
    assert_eq!(done["state"], "completed_with_reply", "{done}");
    assert_eq!(done["execution"]["redispatches"], 0, "{done}");
}

/// Contract 1.64.0: a request whose worker died while it was still `admitted` (nothing
/// written or typed) is dispatched again, once, and delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admitted_orphan_is_dispatched_again_once_and_delivered() {
    let f = fixture_with(true).await;
    let (_claude, id, _reset) = live_pane(&f).await;
    let s = f.session(&id);
    std::fs::write(f.herdr.join("hold-read"), "").unwrap();
    let b = body("submit", &s, "second worker");
    let message = b["message_id"].as_str().unwrap().to_owned();
    let sent = f.send_ok(&b);
    assert_eq!(sent["execution"]["phase"], "admitted", "{sent}");
    let mut pid = None;
    wait_for("delivery worker", || {
        pid = worker_pid(&f, &message);
        pid.is_some()
    });
    unsafe {
        libc::kill(pid.unwrap() as i32, libc::SIGKILL);
    }
    wait_for("worker gone", || worker_pid(&f, &message).is_none());
    std::fs::remove_file(f.herdr.join("hold-read")).unwrap();
    let done = f.settled(&message);
    assert_eq!(done["state"], "completed_with_reply", "{done}");
    assert_eq!(done["execution"]["redispatches"], 1, "{done}");
    assert_eq!(
        done["execution"]["redispatched_at"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// Contract 1.64.0: a worker that died after it started typing (`dispatching`) is never
/// replaced by a second delivery: the request is withdrawn and ends unsent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatching_orphan_is_not_dispatched_again() {
    let f = fixture_with(true).await;
    let (_claude, id, _reset) = live_pane(&f).await;
    let s = f.session(&id);
    std::fs::write(f.herdr.join("hold-send-text"), "").unwrap();
    let b = body("submit", &s, "typed once at most");
    let message = b["message_id"].as_str().unwrap().to_owned();
    f.send_ok(&b);
    let mut pid = None;
    wait_for("worker typing", || {
        pid = worker_pid(&f, &message);
        pid.is_some()
            && read(&f.herdr.join("calls")).contains("send-text")
            && f.api(&["receipt", &message])["execution"]["phase"] == "dispatching"
    });
    unsafe {
        libc::kill(pid.unwrap() as i32, libc::SIGKILL);
    }
    wait_for("worker gone", || worker_pid(&f, &message).is_none());
    std::fs::remove_file(f.herdr.join("hold-send-text")).unwrap();
    let done = f.settled(&message);
    assert_eq!(done["state"], "rejected", "{done}");
    assert_eq!(done["facts"]["written"], false, "{done}");
    assert_eq!(done["execution"]["redispatches"], 0, "{done}");
    assert!(
        !f.writes().iter().any(|w| w.iter().any(|k| k == "enter")),
        "never submitted"
    );
}

#[path = "support/typing_golden.rs"]
mod typing_golden;

/// Step S8 golden: the exact HERDR command sequence of representative Claude deliveries
/// (recorded on main before the typing engine refactor; see `support/typing_golden.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_golden_herdr_sequences_for_claude() {
    let f = fixture().await;
    let (_claude, id, reset) = live_pane(&f).await;
    let mut g = typing_golden::Golden::new(&f.herdr);
    let typed = |mode: &str, text: &str| {
        let mut b = body(mode, &f.session(&id), text);
        b["delivery"] = json!("terminal_input");
        b
    };
    let long = (0..3)
        .map(|i| format!("line {i}: {}", "check the build and the tests ".repeat(3)))
        .collect::<Vec<_>>()
        .join("\n");
    for (name, patch, mode, text) in [
        ("submit idle", json!({}), "submit", "hello there."),
        (
            "steer busy",
            json!({"status":"working"}),
            "steer",
            "also run clippy.",
        ),
        ("multi-line", json!({}), "submit", long.as_str()),
        (
            "escaped leading slash",
            json!({}),
            "submit",
            "/compact the notes.",
        ),
        (
            "draft present",
            json!({"input":"half a thought\nsecond line"}),
            "steer",
            "please check\nthe build.",
        ),
        (
            "withdraw after failed verification",
            json!({"input":"garbled draft","garble_once":true}),
            "steer",
            "will be garbled.",
        ),
        (
            "leave help",
            json!({"view":"help"}),
            "submit",
            "after help.",
        ),
        (
            "leave transcript",
            json!({"view":"transcript"}),
            "submit",
            "after the transcript.",
        ),
        (
            "refused screen",
            json!({"view":"subagent"}),
            "submit",
            "never typed.",
        ),
    ] {
        reset(patch);
        g.begin(name);
        let b = typed(mode, text);
        f.send_ok(&b);
        let r = f.settled(b["message_id"].as_str().unwrap());
        g.end(&r);
    }
    g.begin("queue refused");
    g.refused(&f.send_err(&typed("queue", "later.")));
    g.check("claude");
}
