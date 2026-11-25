//! Claude submit/queue/steer (contract revision 16) with fakes only:
//! `support/fake-claude-peer.py` (a live session with an inbox socket).
//! Disposable homes; no provider network, credentials or model requests.
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
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGKILL);
            }
        }
        let _ = self.0.wait();
    }
}
struct Fixture {
    tmp: tempfile::TempDir,
    paths: Paths,
    project: PathBuf,
    _daemon: Option<Process>,
}
fn start_daemon(paths: &Paths) -> Process {
    Process(
        Command::new(binary())
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["daemon", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}
async fn wait_daemon(paths: &Paths) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon::call(paths, "status", json!({})).await.is_err() {
        assert!(
            Instant::now() < deadline,
            "fixture supervisor did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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
    install(
        "fake-claude-peer.py",
        include_str!("support/fake-claude-peer.py"),
    );
    let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
        ..Default::default()
    };
    sessionsation::store::save_config(&paths, &config).unwrap();
    let daemon = start_daemon(&paths);
    wait_daemon(&paths).await;
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = std::fs::canonicalize(project).unwrap();
    Fixture {
        tmp,
        paths,
        project,
        _daemon: Some(daemon),
    }
}
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn now() -> i64 {
    daemon::now()
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
            .env_remove("SESSIONSATION_SENDER_COMPONENT")
            .env("ANTHROPIC_API_KEY", "sk-test-must-not-reach-claude")
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
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("accounts/claude").join(name)
    }
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
    fn receipt_until(&self, message: &str, f: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        wait_for("receipt condition", || {
            last = self.api(&["receipt", message]);
            f(&last)
        });
        last
    }
}
fn body(mode: &str, session: &Value, text: &str) -> Value {
    json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":mode,"session_id":session["id"],
        "expected_generation":session["generation"],"text":text,"expires_at":now()+600,"await_reply":true})
}
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_messages_need_opt_in_and_claim_replies_only_with_transcript_proof() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let conversation = uuid::Uuid::new_v4().to_string();
    let socket = f.tmp.path().join("p.sock");
    let peer = Process(
        Command::new("python3")
            .arg(f.tmp.path().join("fake-claude-peer.py"))
            .args([
                &conversation,
                f.project.to_str().unwrap(),
                socket.to_str().unwrap(),
            ])
            .env("CLAUDE_CONFIG_DIR", &home)
            .current_dir(&f.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut s = Value::Null;
    wait_for("live peer session", || {
        s = f.session(&conversation);
        s["availability"] == "running"
    });
    let id = s["id"].as_str().unwrap().to_owned();
    assert_eq!(s["capabilities"]["steer"], false);
    assert!(
        s["capability_reasons"]["steer"]
            .as_str()
            .unwrap()
            .starts_with("LIVE_STEERING_DISABLED")
    );
    let err = f.send_err(&body("steer", &s, "x"));
    assert!(err.contains("LIVE_STEERING_DISABLED"), "{err}");
    assert!(
        err.contains("TERMINAL_NOT_ATTACHED"),
        "the ineligible terminal path is named too: {err}"
    );
    assert!(
        !home.join("fake-peer-frames.jsonl").exists(),
        "nothing sent"
    );

    // Opt in: explicit, preserves other keys, logged as an event.
    std::fs::write(home.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
    let out = f.run(&["account", "live-steering", "claude:personal", "--enable"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let toggled: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(toggled["changed"], true);
    assert_eq!(toggled["live_steering"], true);
    let settings: Value = serde_json::from_str(&read(&home.join("settings.json"))).unwrap();
    assert_eq!(
        settings,
        json!({"theme":"dark","crossSessionInbound":"accept"})
    );
    let again = f.api(&["account", "live-steering", "claude:personal", "--enable"]);
    assert_eq!(again["changed"], false);
    let s = f.session(&id);
    assert_eq!(s["capabilities"]["steer"], true, "{s}");
    assert_eq!(s["input_delivery"]["semantics"], "peer_message");

    // Delivered peer message with a transcript-anchored reply.
    let steer = body("steer", &s, "status please");
    let m = steer["message_id"].as_str().unwrap().to_owned();
    let first = f.send_ok(&steer);
    assert_eq!(first["delivery_semantics"], "peer_message");
    assert!(
        first["peer_message"]["label"]
            .as_str()
            .unwrap()
            .contains("partially")
    );
    let r = f.settled(&m);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    let reply = r["reply"]["text"].as_str().unwrap();
    assert!(
        reply.starts_with("peer reply to: <cross-session-message from=\"uds:/"),
        "{reply}"
    );
    assert!(reply.ends_with("\" from-name=\"sessionsation\" from-mode=\"prompting\">\nstatus please\n</cross-session-message>"), "{reply}");
    assert_eq!(r["correlation"]["anchor_found"], true);
    assert_eq!(r["facts"]["peer_status"][0]["status"], "delivered");
    assert_eq!(r["facts"]["idle_notice"], true);
    let frames: Vec<Value> = read(&home.join("fake-peer-frames.jsonl"))
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let user = frames.iter().find(|f| f["type"] == "user").unwrap();
    assert_eq!(user["uuid"], m.as_str());
    assert_eq!(user["msg_id"], m.as_str());
    assert_eq!(user["session_id"], conversation.as_str());
    assert_eq!(user["priority"], "next");
    assert!(user["from"].as_str().unwrap().starts_with("uds:/"));
    assert!(
        frames
            .iter()
            .any(|f| f["type"] == "control" && f["action"] == "notify_when_idle")
    );
    assert_eq!(user["msgV"], 1);
    // The reply socket lives next to the target's inbox socket as <pid>-<8 hex>.sock.
    let from = std::path::Path::new(user["from"].as_str().unwrap().strip_prefix("uds:").unwrap());
    assert_eq!(from.parent().unwrap(), socket.parent().unwrap());
    let name = from.file_name().unwrap().to_str().unwrap();
    let (pid, hex) = name.strip_suffix(".sock").unwrap().split_once('-').unwrap();
    assert!(pid.parse::<u32>().is_ok() && hex.len() == 8, "{name}");
    assert_eq!(r["peer_message"]["reply_socket"], "namespaced");
    assert_eq!(r["peer_message"]["evidence"], "transcript_anchor");
    assert_eq!(r["origin"], "operator");
    assert!(
        !r.to_string().contains("private operator prompt"),
        "user text never read into receipts"
    );
    assert_eq!(f.send_ok(&steer)["state"], "completed_with_reply");
    assert_eq!(
        read(&home.join("fake-peer-frames.jsonl")).lines().count(),
        frames.len(),
        "no resend"
    );
    let mut interrupt = body("steer", &f.session(&id), "x");
    interrupt["steering"] = json!({"version":1,"interrupt":true});
    assert!(
        f.send_err(&interrupt)
            .contains("PEER_INTERRUPT_UNSUPPORTED")
    );

    // Delivered and idle, but no anchor in the transcript: never claimed answered.
    std::fs::write(home.join("fake-peer-mode"), "silent").unwrap();
    let silent = body("submit", &f.session(&id), "quiet one");
    f.send_ok(&silent);
    let r = f.receipt_until(silent["message_id"].as_str().unwrap(), |r| {
        r["facts"]["idle_notice"] == true && r["state"] == "native_accepted"
    });
    assert_eq!(r["state"], "native_accepted", "{r}");
    assert_eq!(r["observation"]["error_code"], "peer_anchor_not_found");
    assert!(r["reply"].is_null());

    // Held (no answer), refused and expired are reported as such.
    std::fs::write(home.join("fake-peer-mode"), "hold").unwrap();
    let mut held = body("queue", &f.session(&id), "wait for me");
    held["timeout_seconds"] = json!(2);
    f.send_ok(&held);
    let r = f.receipt_until(held["message_id"].as_str().unwrap(), |r| {
        r["state"] == "held"
    });
    assert_eq!(r["facts"]["held"], true);
    assert_eq!(r["priority"], "later");
    for (mode, state, reason) in [
        ("refuse", "refused", "PEER_MESSAGE_REFUSED"),
        ("expire", "expired", "PEER_MESSAGE_EXPIRED"),
    ] {
        std::fs::write(home.join("fake-peer-mode"), mode).unwrap();
        let b = body("submit", &f.session(&id), mode);
        f.send_ok(&b);
        let r = f.settled(b["message_id"].as_str().unwrap());
        assert_eq!(r["state"], state, "{r}");
        assert_eq!(r["reason"], reason);
        assert!(r["reply"].is_null());
    }

    // Opt out: key removed, other keys kept, live steering refused again.
    let off = f.api(&["account", "live-steering", "claude:personal", "--disable"]);
    assert_eq!(off["changed"], true);
    let settings: Value = serde_json::from_str(&read(&home.join("settings.json"))).unwrap();
    assert_eq!(settings, json!({"theme":"dark"}));
    assert_eq!(
        f.api(&["account", "live-steering", "claude:personal"])["live_steering"],
        false
    );
    assert!(
        f.send_err(&body("steer", &f.session(&id), "x"))
            .contains("LIVE_STEERING_DISABLED")
    );
    let db =
        rusqlite::Connection::open(f.paths.root.join(sessionsation::layout::REGISTRY_DB)).unwrap();
    let kinds: Vec<String> = db
        .prepare("SELECT kind FROM events WHERE kind LIKE 'account.live_steering%' ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        kinds,
        [
            "account.live_steering_enabled",
            "account.live_steering_disabled"
        ]
    );
    drop(peer);
}

/// Native delivery (contract 1.40.0): an agent-originated send reaches a session that
/// was launched accepting inbound messages without any account setting; every
/// receipt shape of the cross-session protocol is reported honestly; nothing is
/// ever sent twice; a native idle notice is journaled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_sends_use_the_native_peer_protocol_and_report_every_receipt_honestly() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let conversation = uuid::Uuid::new_v4().to_string();
    let socket = f.tmp.path().join("p.sock");
    // Launched the way Sessionsation launches Claude: `--settings` carries the accept.
    let peer = Process(
        Command::new("python3")
            .arg(f.tmp.path().join("fake-claude-peer.py"))
            .args([
                &conversation,
                f.project.to_str().unwrap(),
                socket.to_str().unwrap(),
                "--settings",
                r#"{"crossSessionInbound":"accept"}"#,
            ])
            .env("CLAUDE_CONFIG_DIR", &home)
            .current_dir(&f.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut s = Value::Null;
    wait_for("live peer session", || {
        s = f.session(&conversation);
        s["availability"] == "running"
    });
    let id = s["id"].as_str().unwrap().to_owned();
    assert!(
        !read(&home.join("settings.json")).contains("crossSessionInbound"),
        "no account-level opt-in was needed"
    );
    let frames = || -> Vec<Value> {
        read(&home.join("fake-peer-frames.jsonl"))
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let users = || frames().into_iter().filter(|x| x["type"] == "user").count();
    let agent = |mode: &str, text: &str, delivery: &str| {
        let mut b = body(mode, &f.session(&id), text);
        b["origin"] = json!("agent");
        b["delivery"] = json!(delivery);
        b["timeout_seconds"] = json!(8);
        b
    };

    // Receipt: delivered + transcript anchor.
    let b = agent("submit", "agent hello", "native");
    let r = f.send_ok(&b);
    assert_eq!(r["delivery_semantics"], "peer_message");
    assert_eq!(r["origin"], "agent");
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["peer_message"]["evidence"], "transcript_anchor");
    assert_eq!(r["native_fallback"], Value::Null);
    let user = frames().into_iter().find(|x| x["type"] == "user").unwrap();
    assert_eq!(user["msgV"], 1);
    let content = user["message"]["content"].as_str().unwrap();
    assert!(
        content.starts_with("<cross-session-message from=\"uds:/"),
        "{content}"
    );
    assert!(content.ends_with("\" from-name=\"sessionsation\" from-mode=\"prompting\">\nagent hello\n</cross-session-message>"), "{content}");
    assert_eq!(users(), 1);

    // Contract 1.50.0/1.51.0: an outside-session caller names its component; the request field wins
    // over SESSIONSATION_SENDER_COMPONENT; an invalid env var is ignored with a warning.
    let last_content = || -> String {
        frames()
            .into_iter()
            .rev()
            .find(|x| x["type"] == "user")
            .unwrap()["message"]["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let send_env = |b: &Value, env: Option<&str>| -> Output {
        let mut c = f.command(&["api", "send"]);
        if let Some(e) = env {
            c.env("SESSIONSATION_SENDER_COMPONENT", e);
        }
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    // Contract 1.51.0: the component is rendered bare, unless it names a catalog session
    // (case-insensitively) or is `sessionsation`; then the prefixed form, never refused.
    let own = s["name"].as_str().unwrap().to_uppercase();
    assert!(
        own.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "the fixture session name must be a valid component: {own}"
    );
    let own_prefixed = format!("sessionsation:{own}");
    for (field, env, from_name, warning, prefixed) in [
        (
            Some("resource-guard"),
            Some("env-guard"),
            "resource-guard",
            None,
            false,
        ),
        (None, Some("env-guard"), "env-guard", None, false),
        (
            None,
            Some("bad name"),
            "sessionsation",
            Some("ignoring SESSIONSATION_SENDER_COMPONENT"),
            false,
        ),
        (Some(own.as_str()), None, own_prefixed.as_str(), None, true),
        (
            Some("Sessionsation"),
            None,
            "sessionsation:Sessionsation",
            None,
            true,
        ),
        (
            None,
            Some(own.as_str()),
            own_prefixed.as_str(),
            Some("collides"),
            false,
        ),
    ] {
        let mut b = agent("submit", "component hello", "native");
        if let Some(c) = field {
            b["from_component"] = json!(c);
        }
        let out = send_env(&b, env);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{stderr}");
        assert_eq!(
            stderr.contains("sessionsation: warning:"),
            warning.is_some(),
            "{stderr}"
        );
        if let Some(w) = warning {
            assert!(stderr.contains(w), "{stderr}");
        }
        let r = f.settled(b["message_id"].as_str().unwrap());
        assert_eq!(r["state"], "completed_with_reply", "{r}");
        assert!(r["facts"].get("from_component_ignored").is_none(), "{r}");
        assert_eq!(
            r["facts"].get("from_component_prefixed"),
            prefixed.then(|| json!("collides_with_session")).as_ref(),
            "{r}"
        );
        let content = last_content();
        assert!(
            content.ends_with(&format!("\" from-name=\"{from_name}\" from-mode=\"prompting\">\ncomponent hello\n</cross-session-message>")),
            "{content}"
        );
    }
    // An invalid request field is refused before anything is sent.
    let before = users();
    let mut b = agent("submit", "never sent", "native");
    b["from_component"] = json!("not ok!");
    let err = f.send_err(&b);
    assert!(err.contains("INVALID_ARGUMENT"), "{err}");
    assert!(err.contains("from_component"), "{err}");
    assert_eq!(users(), before);

    // Real-receiver success is silent: the transcript anchor upgrades it.
    std::fs::write(home.join("fake-peer-mode"), "quiet").unwrap();
    let b = agent("submit", "quiet success", "auto");
    f.send_ok(&b);
    let r = f.settled(b["message_id"].as_str().unwrap());
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(r["facts"]["peer_status"], json!([]));
    assert_eq!(r["facts"]["input_recorded"], true);

    // Silent and unproven: transport-accepted only, never "read".
    std::fs::write(home.join("fake-peer-mode"), "quiet-nothing").unwrap();
    let b = agent("submit", "into the void", "native");
    f.send_ok(&b);
    let r = f.receipt_until(b["message_id"].as_str().unwrap(), |r| {
        r["state"] == "native_accepted" && r["observation"]["error_code"] == "peer_anchor_not_found"
    });
    assert_eq!(r["peer_message"]["evidence"], "transport_accepted_silent");
    assert_eq!(r["facts"]["input_recorded"], false);
    assert_eq!(r["facts"]["input_consumed"], "unknown");
    assert!(r["reply"].is_null());

    // Denied, dropped, refused: failed with a reason.
    let mut dropped_id = String::new();
    for (mode, state, reason) in [
        ("denied", "refused", "PEER_MESSAGE_DENIED"),
        ("drop", "refused", "PEER_MESSAGE_DROPPED"),
        ("refuse", "refused", "PEER_MESSAGE_REFUSED"),
    ] {
        std::fs::write(home.join("fake-peer-mode"), mode).unwrap();
        let b = agent("submit", mode, "native");
        f.send_ok(&b);
        let r = f.settled(b["message_id"].as_str().unwrap());
        assert_eq!(
            (r["state"].as_str(), r["reason"].as_str()),
            (Some(state), Some(reason)),
            "{r}"
        );
        if mode == "drop" {
            dropped_id = b["message_id"].as_str().unwrap().to_owned();
        }
    }
    let dropped = f.receipt_until(&dropped_id, |_| true);
    assert_eq!(
        dropped["facts"]["peer_status"][0]["drop_reason"],
        "queue-full"
    );

    // Held by a bypass-permission receiver.
    std::fs::write(home.join("fake-peer-mode"), "hold").unwrap();
    let b = agent("queue", "held one", "native");
    f.send_ok(&b);
    let r = f.receipt_until(b["message_id"].as_str().unwrap(), |r| r["state"] == "held");
    assert_eq!(r["facts"]["held"], true);

    // A receiver that requires an auth line drops silently: nothing is claimed,
    // and the message is not sent again by any other path.
    std::fs::write(home.join("fake-peer-mode"), "auth").unwrap();
    let before = users();
    let b = agent("submit", "needs auth", "native");
    f.send_ok(&b);
    let r = f.receipt_until(b["message_id"].as_str().unwrap(), |r| {
        r["state"] == "native_accepted"
    });
    assert_eq!(r["facts"]["input_recorded"], false);
    assert_eq!(r["native_fallback"], Value::Null);
    assert_eq!(users(), before, "the dropped connection delivered nothing");

    // Strict peer_message never falls back; an unknown origin is refused.
    let mut bad = agent("submit", "x", "native");
    bad["origin"] = json!("robot");
    assert!(
        f.send_err(&bad)
            .contains("origin must be operator or agent")
    );

    std::fs::write(home.join("fake-peer-mode"), "accept").unwrap();
    // Origin: omitted from a plain shell = operator (default); explicit is recorded as such;
    // omitted from inside a managed session's process tree = agent (detected, contract 1.41.0).
    std::fs::write(home.join("fake-peer-mode"), "accept").unwrap();
    let mut plain = body("submit", &f.session(&id), "no origin given");
    plain["timeout_seconds"] = json!(8);
    let r = f.send_ok(&plain);
    assert_eq!(
        (r["origin"].as_str(), r["origin_source"].as_str()),
        (Some("operator"), Some("default")),
        "{r}"
    );
    f.settled(plain["message_id"].as_str().unwrap());
    let b = agent("submit", "explicit agent", "auto");
    let r = f.send_ok(&b);
    assert_eq!(
        (r["origin"].as_str(), r["origin_source"].as_str()),
        (Some("agent"), Some("explicit")),
        "{r}"
    );
    f.settled(b["message_id"].as_str().unwrap());
    let mut detected = body("submit", &f.session(&id), "detected agent");
    detected["timeout_seconds"] = json!(8);
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    sessionsation::catalog::record_provider_launch(
        &f.paths,
        sessionsation::provider::ProviderId::Claude,
        "personal",
        &uuid::Uuid::new_v4().to_string(),
        "the-sender",
        None,
        Some(sessionsation::catalog::Attachment {
            host: "herdr/x".into(),
            target: "herdr/x/w:p".into(),
            pid: std::process::id(),
            process_start: start,
        }),
    )
    .unwrap();
    let r = f.send_ok(&detected);
    assert_eq!(
        (r["origin"].as_str(), r["origin_source"].as_str()),
        (Some("agent"), Some("detected")),
        "{r}"
    );
    assert_eq!(r["delivery_semantics"], "peer_message");
    f.settled(detected["message_id"].as_str().unwrap());
    drop(peer);
}
