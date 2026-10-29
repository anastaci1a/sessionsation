//! Claude submit/queue/steer (contract revision 16) with fakes only:
//! `support/fake-claude-stream.py` (hosted stream-json provider) and
//! `support/fake-claude-peer.py` (a live session with an inbox socket).
//! Disposable homes; no provider network, credentials or model requests.
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
    daemon: Option<Process>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Stop any hosted worker this fixture started (best effort).
        if let Ok(entries) = std::fs::read_dir(self.paths.root.join("claude-hosted")) {
            for e in entries.flatten() {
                if let Ok(bytes) = std::fs::read(e.path().join("host.json"))
                    && let Ok(h) = serde_json::from_slice::<Value>(&bytes)
                {
                    for pid in [h["worker_pid"].as_i64(), h["child_pid"].as_i64()]
                        .into_iter()
                        .flatten()
                    {
                        unsafe {
                            libc::kill(pid as i32, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
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
        "fake-claude-stream.py",
        include_str!("support/fake-claude-stream.py"),
    );
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
        daemon: Some(daemon),
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
    fn api_error(&self, args: &[&str]) -> String {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("claude-accounts").join(name)
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
    fn hosted_status(&self, session: &str) -> Value {
        self.api(&["hosted", "status", "--session", session])
    }
}
fn body(mode: &str, session: &Value, text: &str) -> Value {
    json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":mode,"session_id":session["id"],
        "expected_generation":session["generation"],"text":text,"expires_at":now()+600})
}
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
/// The fake provider's launch record for this conversation (written after it starts).
fn launched(home: &Path, conversation: &str) -> Value {
    let mut v = Value::Null;
    wait_for("provider launch record", || {
        v = serde_json::from_str(&read(&home.join("fake-stream-launch.json")))
            .unwrap_or(Value::Null);
        v["session"] == conversation
    });
    v
}
fn worker_gone(f: &Fixture, conversation: &str) -> bool {
    let lock = f
        .paths
        .root
        .join("claude-hosted")
        .join(conversation)
        .join("worker.lock");
    let file = std::fs::OpenOptions::new().write(true).open(&lock);
    file.is_err() || fs2::FileExt::try_lock_exclusive(&file.unwrap()).is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_sessions_correlate_replies_fold_inputs_and_keep_one_writer() {
    let mut f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let project = f.project.to_str().unwrap().to_owned();
    let started = f.api(&[
        "hosted",
        "start",
        "--account",
        "claude:personal",
        "--directory",
        &project,
        "--name",
        "hosted-one",
    ]);
    assert_eq!(started["state"], "running", "{started}");
    assert_eq!(started["permission_mode"], "account");
    assert!(started["effective_permission_mode"].is_null());
    let id = started["session_id"].as_str().unwrap().to_owned();
    let conversation = started["conversation_id"].as_str().unwrap().to_owned();
    let launch = launched(&home, &conversation);
    let args: Vec<String> = serde_json::from_value(launch["args"].clone()).unwrap();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--session-id" && w[1] == conversation)
    );
    assert!(
        !args
            .iter()
            .any(|a| a.contains("bypass") || a == "--permission-mode"),
        "{args:?}"
    );
    assert_eq!(
        launch["api_key"], "",
        "billing overrides never reach the provider"
    );
    assert_eq!(launch["autoupdater"], "1");
    assert_eq!(launch["cwd"], project.as_str());

    // The session row reports the host and per-session input capabilities.
    let s = f.session(&id);
    assert_eq!(s["availability"], "running", "{s}");
    assert_eq!(s["hosted"]["state"], "running");
    assert_eq!(s["active_since"], s["hosted"]["started_at"], "{s}");
    assert!(s["active_since"].is_i64(), "{s}");
    assert_eq!(s["capabilities"]["steer"], true);
    assert_eq!(s["capabilities"]["resume"], false);
    assert_eq!(s["input_delivery"]["semantics"], "hosted_stream_json");
    assert!(s["capability_reasons"]["submit"].is_null());

    // Submit: correlated reply between started and the result for this uuid.
    let hello = body("submit", &s, "hello there");
    let m1 = hello["message_id"].as_str().unwrap().to_owned();
    let first = f.send_ok(&hello);
    assert_eq!(first["delivery_semantics"], "hosted_stream_json");
    assert_eq!(first["priority"], "next");
    let r = f.settled(&m1);
    assert_eq!(r["state"], "completed_with_reply", "{r}");
    assert_eq!(
        r["reply"]["text"],
        "working on: hello there\n\nfake reply to: hello there"
    );
    assert_eq!(r["reply"]["final"], true);
    assert_eq!(r["reply"]["is_error"], false);
    assert_eq!(r["correlation"]["command_uuid"], m1.as_str());
    assert_eq!(r["correlation"]["user_message_uuids"], json!([m1]));
    assert_eq!(r["facts"]["input_consumed"], true);
    assert_eq!(r["execution"]["automatic_replay"], false);
    let lifecycle = r["facts"]["lifecycle"].to_string();
    assert!(
        lifecycle.contains("queued") && lifecycle.contains("started"),
        "{lifecycle}"
    );
    // Idempotent retry returns the original receipt; a changed payload conflicts.
    assert_eq!(f.send_ok(&hello)["reply"], r["reply"]);
    let mut changed = hello.clone();
    changed["text"] = json!("different");
    assert!(f.send_err(&changed).contains("MESSAGE_ID_CONFLICT"));
    let events = f.api(&["hosted", "events", "--session", &id]);
    let echoes = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "user" && e["text"] == "hello there")
        .count();
    assert_eq!(echoes, 1, "never written twice");

    // Steer during a running turn (Claude Code 2.1.312 semantics): sent as
    // priority `next`, it never cuts the turn; it is folded in at the turn's
    // next tool round. Its reply starts at its own `started`, both receipts
    // complete normally and say the reply is shared.
    assert_eq!(
        f.session(&id)["input_delivery"]["steer"]["interrupts"],
        false
    );
    assert_eq!(
        f.session(&id)["input_delivery"]["steer"]["priority"],
        "next"
    );
    let a = body("submit", &f.session(&id), "slow alpha");
    let b = body("steer", &f.session(&id), "beta");
    f.send_ok(&a);
    let ma = a["message_id"].as_str().unwrap().to_owned();
    f.receipt_until(&ma, |r| r["state"] == "execution_started");
    let steer = f.send_ok(&b);
    assert_eq!(steer["priority"], "next");
    let mb = b["message_id"].as_str().unwrap().to_owned();
    let ra = f.settled(&ma);
    let rb = f.settled(&mb);
    assert_eq!(ra["state"], "completed_with_reply", "{ra}");
    assert_eq!(rb["state"], "completed_with_reply", "{rb}");
    assert_eq!(
        ra["reply"]["text"],
        "working on: slow alpha\n\nfake reply to: slow alpha\n\nnoted: beta"
    );
    assert_eq!(
        rb["reply"]["text"],
        "fake reply to: slow alpha\n\nnoted: beta"
    );
    assert_eq!(rb["facts"]["folded"], true);
    assert_eq!(rb["facts"]["shared_with_turn"], true);
    assert_eq!(rb["correlation"]["user_message_uuids"], json!([ma, mb]));
    assert_eq!(ra["reply"]["final"], true);
    assert_eq!(rb["reply"]["final"], true);
    assert_eq!(
        ra["correlation"]["lifecycle"],
        json!(["queued", "started", "completed"])
    );
    assert_eq!(
        rb["correlation"]["lifecycle"],
        json!(["queued", "started", "completed"])
    );
    assert_eq!(ra["facts"]["folded"], false);

    // A steer to a running turn that has no further tool round is not folded:
    // the turn completes unchanged and the steer runs as the next turn, each
    // reply anchored to its own input.
    let long = body("submit", &f.session(&id), "long theta");
    f.send_ok(&long);
    let ml = long["message_id"].as_str().unwrap().to_owned();
    f.receipt_until(&ml, |r| r["state"] == "execution_started");
    let late = body("steer", &f.session(&id), "iota");
    f.send_ok(&late);
    let mi = late["message_id"].as_str().unwrap().to_owned();
    let rl = f.settled(&ml);
    let ri = f.settled(&mi);
    assert_eq!(rl["state"], "completed_with_reply", "{rl}");
    assert_eq!(
        rl["reply"]["text"],
        "working on: long theta\n\nfake reply to: long theta"
    );
    assert_eq!(rl["reply"]["final"], true);
    assert_eq!(rl["facts"]["shared_with_turn"], false);
    assert_eq!(rl["correlation"]["user_message_uuids"], json!([ml]));
    assert_eq!(ri["state"], "completed_with_reply", "{ri}");
    assert_eq!(
        ri["reply"]["text"],
        "working on: iota\n\nfake reply to: iota"
    );
    assert_eq!(ri["facts"]["folded"], false);
    assert_eq!(ri["correlation"]["user_message_uuids"], json!([mi]));

    // A turn Claude Code cuts (as for another client's `now` input) still
    // emits a `success` result; only the `cancelled` lifecycle after it
    // tells. The submit is never completed: `interrupted`, partial reply
    // kept but not final, nothing replayed.
    let cut = body("submit", &f.session(&id), "long cut-by-now kappa");
    f.send_ok(&cut);
    let mk = cut["message_id"].as_str().unwrap().to_owned();
    let rk = f.settled(&mk);
    assert_eq!(rk["state"], "interrupted", "{rk}");
    assert_eq!(rk["reason"], "TURN_CANCELLED_BY_PROVIDER");
    assert_eq!(rk["reply"]["text"], "working on: long cut-by-now kappa");
    assert_eq!(rk["reply"]["final"], false);
    assert_eq!(rk["reply"]["is_error"], false);
    assert_eq!(rk["facts"]["reply_final"], false);
    assert_eq!(rk["facts"]["input_consumed"], true);
    assert_eq!(
        rk["correlation"]["lifecycle"],
        json!(["queued", "started", "cancelled"])
    );
    assert_eq!(rk["correlation"]["user_message_uuids"], json!([mk]));
    assert_eq!(rk["execution"]["automatic_replay"], false);

    // Queue (later) and a native refusal.
    let q = body("queue", &f.session(&id), "queued item");
    f.send_ok(&q);
    let rq = f.settled(q["message_id"].as_str().unwrap());
    assert_eq!(rq["priority"], "later");
    assert_eq!(rq["state"], "completed_with_reply");
    let no = body("submit", &f.session(&id), "refuse-me");
    f.send_ok(&no);
    let rn = f.settled(no["message_id"].as_str().unwrap());
    assert_eq!(rn["state"], "refused", "{rn}");
    assert_eq!(rn["reason"], "NATIVE_REFUSED");
    assert!(rn["reply"].is_null());

    // Interrupt: the running turn's submit ends `interrupted` (partial reply,
    // not final); the steer gets its own reply.
    let c = body("submit", &f.session(&id), "slow gamma");
    f.send_ok(&c);
    let mc = c["message_id"].as_str().unwrap().to_owned();
    f.receipt_until(&mc, |r| r["state"] == "execution_started");
    // Accepted, in-flight messages cannot be cancelled.
    assert!(
        f.api_error(&["cancel", &mc])
            .contains("STEERING_CANCELLATION_UNSUPPORTED")
    );
    let mut d = body("steer", &f.session(&id), "delta");
    d["steering"] = json!({"version":1,"interrupt":true});
    f.send_ok(&d);
    let rc = f.settled(&mc);
    let rd = f.settled(d["message_id"].as_str().unwrap());
    assert_eq!(rc["state"], "interrupted", "{rc}");
    assert_eq!(rc["reason"], "TURN_INTERRUPTED");
    assert_eq!(rc["reply"]["text"], "working on: slow gamma");
    assert_eq!(rc["reply"]["final"], false);
    assert_eq!(rc["facts"]["reply_final"], false);
    assert!(rc["facts"]["lifecycle"].to_string().contains("cancelled"));
    assert_eq!(
        rd["correlation"]["user_message_uuids"],
        json!([d["message_id"]])
    );
    assert_eq!(rd["state"], "completed_with_reply", "{rd}");
    assert_eq!(rd["facts"]["interrupt_requested"], true);
    assert_eq!(
        rd["reply"]["text"],
        "working on: delta\n\nfake reply to: delta"
    );
    // Codex-style steering targets are rejected for Claude sessions.
    let mut codex = body("steer", &f.session(&id), "x");
    codex["steering"] =
        json!({"version":1,"expected_turn_id":"t","expected_runtime_generation":"a".repeat(64)});
    assert!(f.send_err(&codex).contains("INVALID_REQUEST"));

    // Permission prompts stay pending until the operator answers.
    let p = body("submit", &f.session(&id), "needs-permission please");
    f.send_ok(&p);
    let mp = p["message_id"].as_str().unwrap().to_owned();
    let mut pending = Value::Null;
    wait_for("pending permission", || {
        pending = f.hosted_status(&id)["pending_permissions"].clone();
        pending.as_array().is_some_and(|a| a.len() == 1)
    });
    assert_eq!(pending[0]["tool_name"], "Bash");
    assert_eq!(f.session(&id)["hosted"]["pending_permission_count"], 1);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        f.api(&["receipt", &mp])["state"],
        "execution_started",
        "never auto-approved"
    );
    let request = pending[0]["request_id"].as_str().unwrap().to_owned();
    f.api(&[
        "hosted",
        "respond",
        "--session",
        &id,
        "--request-id",
        &request,
        "--allow",
    ]);
    let rp = f.settled(&mp);
    assert_eq!(rp["state"], "completed_with_reply", "{rp}");
    assert!(
        rp["reply"]["text"]
            .as_str()
            .unwrap()
            .ends_with("permission allow")
    );
    assert!(
        f.hosted_status(&id)["pending_permissions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // One writer: no interactive resume, headless resume or second host while hosted.
    let s = f.session(&id);
    let generation = s["generation"].as_str().unwrap().to_owned();
    let mut resume = body("resume", &s, "x");
    resume.as_object_mut().unwrap().remove("mode");
    resume["mode"] = json!("resume");
    assert!(f.send_err(&resume).contains("SESSION_ALREADY_RUNNING"));
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
    assert!(
        f.api_error(&[
            "hosted",
            "start",
            "--session",
            &id,
            "--expected-generation",
            &generation
        ])
        .contains("SESSION_ALREADY_RUNNING")
    );
    let human = f.run(&["resume", &id, "--expected-generation", &generation]);
    assert!(!human.status.success());
    assert!(String::from_utf8_lossy(&human.stderr).contains("SESSION_ALREADY_RUNNING"));

    // Supervisor restart: the detached worker is re-adopted, not duplicated.
    let worker = f.hosted_status(&id)["worker"]["pid"].clone();
    drop(f.daemon.take());
    f.daemon = Some(start_daemon(&f.paths));
    wait_daemon(&f.paths).await;
    assert_eq!(f.hosted_status(&id)["worker"]["pid"], worker);
    assert_eq!(f.hosted_status(&id)["worker"]["alive"], true);
    let after = body("submit", &f.session(&id), "after restart");
    f.send_ok(&after);
    assert_eq!(
        f.settled(after["message_id"].as_str().unwrap())["state"],
        "completed_with_reply"
    );
    assert_eq!(
        read(&home.join("fake-stream-count")).trim(),
        "1",
        "one provider process"
    );

    // Clean stop, then hosting the stopped conversation again (resume origin).
    let stopped = f.api(&[
        "hosted",
        "stop",
        "--session",
        &id,
        "--expected-generation",
        f.session(&id)["generation"].as_str().unwrap(),
    ]);
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    assert_eq!(stopped["reason"], "STOPPED");
    assert!(worker_gone(&f, &conversation));
    let s = f.session(&id);
    assert_eq!(s["availability"], "stopped", "{s}");
    assert_eq!(s["capabilities"]["submit"], false);
    assert!(
        s["capability_reasons"]["submit"]
            .as_str()
            .unwrap()
            .starts_with("SESSION_NOT_RUNNING")
    );
    let generation = s["generation"].as_str().unwrap().to_owned();
    let again = f.api(&[
        "hosted",
        "start",
        "--session",
        &id,
        "--expected-generation",
        &generation,
        "--permission-mode",
        "plan",
    ]);
    assert_eq!(again["state"], "running", "{again}");
    assert_eq!(again["origin"], "resume");
    assert_eq!(again["epoch"], 2);
    wait_for("resumed provider", || {
        launched(&home, &conversation)["mode"] == "resume"
    });
    let launch = launched(&home, &conversation);
    let args: Vec<String> = serde_json::from_value(launch["args"].clone()).unwrap();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--resume" && w[1] == conversation)
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "plan")
    );
    assert!(
        f.api_error(&[
            "hosted",
            "start",
            "--account",
            "claude:personal",
            "--directory",
            &project,
            "--permission-mode",
            "yolo"
        ])
        .contains("INVALID_PERMISSION_MODE")
    );

    // Take over: stop the worker cleanly, then the interactive client resumes here.
    let take = Process(
        f.command(&["take-over", &id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = take.0.id();
    wait_for("interactive take-over", || {
        let s = f.session(&id);
        s["availability"] == "running" && s["pid"] == pid && s["hosted"]["state"] == "stopped"
    });
    let interactive = read(&home.join("fake-launch-resume"));
    assert!(
        interactive.contains(&format!("session={conversation}")),
        "{interactive}"
    );
    assert!(worker_gone(&f, &conversation));
    // While the interactive client runs, it is not hosted and needs live steering opt-in.
    let s = f.session(&id);
    assert!(
        s["capability_reasons"]["steer"]
            .as_str()
            .unwrap()
            .starts_with("LIVE_STEERING_DISABLED")
    );
    drop(take);
    let _ = &f.tmp;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_crashes_are_uncertain_and_never_replayed() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let home = f.home("personal");
    let project = f.project.to_str().unwrap().to_owned();
    let started = f.api(&[
        "hosted",
        "start",
        "--account",
        "claude:personal",
        "--directory",
        &project,
    ]);
    let id = started["session_id"].as_str().unwrap().to_owned();
    let conversation = started["conversation_id"].as_str().unwrap().to_owned();

    // Provider crash after the turn started: failed, never success or replay.
    let crash = body("submit", &f.session(&id), "crash-now");
    f.send_ok(&crash);
    let m = crash["message_id"].as_str().unwrap().to_owned();
    let r = f.settled(&m);
    assert_eq!(r["state"], "failed", "{r}");
    assert_eq!(r["reason"], "PROVIDER_EXITED_3_DURING_TURN");
    wait_for("host stopped", || {
        f.hosted_status(&id)["state"] == "stopped"
    });
    assert_eq!(f.hosted_status(&id)["reason"], "PROVIDER_EXITED_3");
    assert_eq!(
        f.send_ok(&crash)["state"],
        "failed",
        "retry returns the receipt"
    );
    assert_eq!(read(&home.join("fake-stream-count")).trim(), "1");
    // Not hosted any more: a stopped session refuses submit with a reason.
    let err = f.send_err(&body("submit", &f.session(&id), "x"));
    assert!(err.contains("SESSION_NOT_RUNNING"), "{err}");

    // Worker crash mid-turn: the draining provider is never given a second
    // writer; the delivery is uncertain/failed and never replayed.
    let s = f.session(&id);
    let restarted = f.api(&[
        "hosted",
        "start",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
    ]);
    assert_eq!(restarted["state"], "running", "{restarted}");
    let slow = body("submit", &f.session(&id), "slow epsilon");
    f.send_ok(&slow);
    let ms = slow["message_id"].as_str().unwrap().to_owned();
    f.receipt_until(&ms, |r| r["state"] == "execution_started");
    let worker = f.hosted_status(&id)["worker"]["pid"].as_i64().unwrap();
    unsafe {
        libc::kill(worker as i32, libc::SIGKILL);
    }
    let draining = f.hosted_status(&id);
    assert!(
        matches!(draining["state"].as_str(), Some("stopping" | "stopped")),
        "{draining}"
    );
    if draining["state"] == "stopping" {
        assert_eq!(draining["reason"], "WORKER_LOST_PROVIDER_DRAINING");
        let s = f.session(&id);
        assert!(
            f.api_error(&[
                "hosted",
                "start",
                "--session",
                &id,
                "--expected-generation",
                s["generation"].as_str().unwrap()
            ])
            .contains("SESSION_ALREADY_RUNNING"),
            "no second writer while the old provider runs"
        );
    }
    wait_for("orphan drained", || {
        f.hosted_status(&id)["state"] == "stopped"
    });
    assert_eq!(f.hosted_status(&id)["reason"], "WORKER_LOST");
    let r = f.settled(&ms);
    assert_eq!(r["state"], "failed", "{r}");
    assert_eq!(r["reason"], "HOST_WORKER_EXITED_DURING_TURN");
    assert_eq!(f.send_ok(&slow)["state"], "failed");
    let transcript = std::fs::read_dir(home.join("projects"))
        .unwrap()
        .flatten()
        .map(|d| d.path().join(format!("{conversation}.jsonl")))
        .find(|p| p.exists())
        .unwrap();
    let inputs = read(&transcript)
        .lines()
        .filter(|l| l.contains("\"type\": \"user\"") && l.contains("slow epsilon"))
        .count();
    assert_eq!(inputs, 1, "the uncertain message was never resent");
    assert_eq!(read(&home.join("fake-stream-count")).trim(), "2");
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
    assert_eq!(r["reply"]["text"], "peer reply to: status please");
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
    assert!(frames.iter().any(|f| f["type"] == "notify_when_idle"));
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
        r["facts"]["idle_notice"] == true
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
    let db = rusqlite::Connection::open(f.paths.root.join("registry.db")).unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_launch_plans_and_terminal_viewer_render_the_event_log() {
    let f = fixture().await;
    f.add_logged_in("personal");
    let plan = f.api(&[
        "launcher",
        "new",
        "--account",
        "claude:personal",
        "--hosted",
        "--permission-mode",
        "acceptEdits",
    ]);
    let argv: Vec<String> = serde_json::from_value(plan["argv"].clone()).unwrap();
    assert!(
        argv.ends_with(&[
            "run".into(),
            "--account".into(),
            "claude:personal".into(),
            "--hosted".into(),
            "--permission-mode".into(),
            "acceptEdits".into()
        ]),
        "{argv:?}"
    );
    // `run --hosted` starts the worker and execs the viewer in this terminal.
    let viewer = Process(
        f.command(&["run", "--account", "claude:personal", "--hosted"])
            .current_dir(&f.project)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut s = Value::Null;
    wait_for("hosted session", || {
        s = f.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["hosted"]["state"] == "running")
            .cloned()
            .unwrap_or(Value::Null);
        !s.is_null()
    });
    let id = s["id"].as_str().unwrap().to_owned();
    let conversation = s["conversation_id"].as_str().unwrap().to_owned();
    // The viewer process (exec'd in place) is what the terminal runs.
    wait_for("viewer exec", || {
        std::fs::read(format!("/proc/{}/cmdline", viewer.0.id())).is_ok_and(|c| {
            let args: Vec<_> = c
                .split(|b| *b == 0)
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            args.windows(2)
                .any(|w| w[0] == "view" && w[1] == conversation)
        })
    });
    let m = body("submit", &s, "render me");
    f.send_ok(&m);
    f.settled(m["message_id"].as_str().unwrap());
    let once = f.run(&["view", &id, "--once"]);
    assert!(
        once.status.success(),
        "{}",
        String::from_utf8_lossy(&once.stderr)
    );
    let text = String::from_utf8_lossy(&once.stdout);
    assert!(text.contains("== hosted Claude session started"), "{text}");
    assert!(text.contains("> render me"), "{text}");
    assert!(text.contains("fake reply to: render me"), "{text}");
    assert!(text.contains("-- turn completed"), "{text}");
    // Closing the viewer does not stop the hosted worker; stop is explicit.
    drop(viewer);
    assert_eq!(f.hosted_status(&id)["worker"]["alive"], true);
    let s = f.session(&id);
    let stopped = f.api(&[
        "hosted",
        "stop",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
    ]);
    assert_eq!(stopped["state"], "stopped");
    let s = f.session(&id);
    let plan = f.api(&[
        "launcher",
        "resume",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--hosted",
    ]);
    let argv: Vec<String> = serde_json::from_value(plan["argv"].clone()).unwrap();
    assert!(
        argv.ends_with(&[
            "run".into(),
            "--account".into(),
            "claude:personal".into(),
            "--thread".into(),
            conversation.clone(),
            "--hosted".into()
        ]),
        "{argv:?}"
    );
    assert_eq!(plan["cwd"], f.project.to_str().unwrap());
}
