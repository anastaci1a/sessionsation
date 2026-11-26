//! Contract 1.61.0: `session launch --brief-delivery initial-prompt` for Codex and
//! `api stop --wait` for Codex.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn run(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> (bool, Value) {
    let mut c = Command::new(binary());
    c.env("TMUX", format!("{},1,0", f.socket))
        .env(
            "PATH",
            format!(
                "{}:{}",
                f.tmp.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .arg("--data-dir")
        .arg(&f.paths.root)
        .arg("api")
        .args(args);
    for (k, v) in envs {
        c.env(k, v);
    }
    let o = c.output().unwrap();
    let text = if o.stdout.is_empty() {
        &o.stderr
    } else {
        &o.stdout
    };
    let v = serde_json::from_slice(text)
        .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(text)));
    (o.status.success(), v)
}

fn session(f: &Fixture) -> Value {
    f.invoke(&["api", "state"])["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == ID)
        .unwrap()
        .clone()
}

fn stop(f: &Fixture, extra: &[&str], envs: &[(&str, &str)]) -> (bool, Value) {
    let s = session(f);
    let mut args = vec![
        "stop",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        s["generation"].as_str().unwrap(),
    ];
    args.extend(extra);
    run(f, &args, envs)
}

fn auto_start(f: &Fixture, mode: &str) {
    let s = session(f);
    let on = f.invoke(&[
        "api",
        "auto-start",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--mode",
        mode,
    ]);
    assert_eq!(on["result"]["auto_start"]["mode"], mode, "{on}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_initial_prompt_delivery_is_first_turn() {
    let f = Fixture::new().await;
    let socket = f.tmp.path().join("host.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let script = f.tmp.path().join("herdr");
    fs::write(
        &script,
        "#!/bin/sh\ncase \"$1 $2\" in\n 'workspace list') printf '%s\\n' '{\"result\":{\"workspaces\":[{\"workspace_id\":\"w1\",\"label\":\"fixture\"}]}}';;\n *) exit 42;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let brief = f.tmp.path().join("brief");
    fs::write(&brief, "do the thing").unwrap();
    let launch = |delivery: Option<&str>| {
        let mut args = vec![
            "session",
            "launch",
            "--space",
            "w1",
            "--name",
            "parity",
            "--account",
            "codex:personal",
            "--model",
            "gpt-6-astra",
            "--brief-file",
            brief.to_str().unwrap(),
            "--directory",
            f.tmp.path().to_str().unwrap(),
            "--herdr-session",
            "fixture",
            "--herdr-socket",
            socket.to_str().unwrap(),
            "--dry-run",
        ];
        if let Some(d) = delivery {
            args.extend(["--brief-delivery", d]);
        }
        run(&f, &args, &[])
    };
    let (ok, v) = launch(Some("initial-prompt"));
    assert!(ok, "{v}");
    assert_eq!(v["result"]["brief"]["delivery"], "first_turn", "{v}");
    assert_eq!(
        v["result"]["brief"]["requested_delivery"], "initial_prompt",
        "{v}"
    );
    // Without the flag nothing was requested.
    let (ok, v) = launch(None);
    assert!(ok, "{v}");
    assert_eq!(v["result"]["brief"]["delivery"], "first_turn", "{v}");
    assert!(v["result"]["brief"]["requested_delivery"].is_null(), "{v}");
    // The Codex size limit still applies.
    fs::write(&brief, "a".repeat(16385)).unwrap();
    let (ok, v) = launch(Some("initial-prompt"));
    assert!(!ok);
    assert_eq!(v["error"]["code"], "INPUT_TOO_LARGE", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_wait_returns_after_the_terminate_job_is_complete() {
    let f = Fixture::new().await;
    f.live(ID, false);
    let (ok, v) = stop(&f, &["--wait"], &[]);
    assert!(ok, "{v}");
    assert_eq!(v["result"]["kind"], "terminate", "{v}");
    assert_eq!(v["result"]["phase"], "complete", "{v}");
    // The journal agrees at the moment the call returned.
    let id = v["result"]["id"].as_str().unwrap();
    let jobs = f.invoke(&["moves"]);
    let job = jobs
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == id)
        .unwrap();
    assert_eq!(job["phase"], "complete", "{job}");
    // Already stopped: the same result with or without --wait.
    let (ok, again) = stop(&f, &["--wait"], &[]);
    assert!(ok, "{again}");
    assert_eq!(again["result"]["already_stopped"], true, "{again}");
    assert_eq!(again["result"]["phase"], "complete");
    let (ok, plain) = stop(&f, &[], &[]);
    assert!(ok, "{plain}");
    assert_eq!(plain["result"], again["result"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_wait_times_out_and_does_not_disable_auto_start() {
    let f = Fixture::new().await;
    // A busy turn keeps the stop waiting for idle.
    f.live(ID, true);
    auto_start(&f, "start");
    let (ok, v) = stop(
        &f,
        &["--wait", "--disable-auto-start"],
        &[("SESSIONSATION_OPERATION_WAIT_MS", "800")],
    );
    assert!(!ok, "{v}");
    assert_eq!(v["error"]["code"], "OPERATION_TIMEOUT", "{v}");
    assert_eq!(v["error"]["retryable"], true, "{v}");
    assert_eq!(v["error"]["operation"]["kind"], "terminate", "{v}");
    assert_ne!(v["error"]["operation"]["phase"], "complete", "{v}");
    // Not disabled at admission: the stop did not finish.
    assert_eq!(session(&f)["auto_start"]["mode"], "start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_wait_disables_auto_start_after_completion() {
    let f = Fixture::new().await;
    f.live(ID, false);
    auto_start(&f, "start");
    let (ok, v) = stop(&f, &["--wait", "--disable-auto-start"], &[]);
    assert!(ok, "{v}");
    assert_eq!(v["result"]["phase"], "complete", "{v}");
    assert_eq!(v["result"]["auto_start"]["mode"], "disabled", "{v}");
    assert_eq!(session(&f)["auto_start"]["mode"], "disabled");
}

// -------------------------------- contract 1.63.0: observe parity (SM4), one stop shape (P1)

#[path = "../support/parity_keys.rs"]
mod parity_keys;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_result_keys_are_identical_per_provider() {
    let f = Fixture::new().await;
    f.live(ID, false);
    let id = session(&f)["id"].as_str().unwrap().to_owned();
    let (ok, v) = stop(&f, &["--wait"], &[]);
    assert!(ok, "{v}");
    let done = &v["result"];
    parity_keys::assert_stop_shape(done, &id, "codex");
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["already_stopped"], false);
    assert_eq!(done["signal"], "SIGTERM");
    assert!(
        done["pid"].is_u64() && done["process_start"].is_u64(),
        "{done}"
    );
    assert!(done["auto_start"].is_null());
    let (ok, again) = stop(&f, &[], &[]);
    assert!(ok, "{again}");
    parity_keys::assert_stop_shape(&again["result"], &id, "codex");
    assert_eq!(again["result"]["already_stopped"], true);
    assert!(again["result"]["pid"].is_null());
    assert_eq!(again["result"]["source_account_id"], "codex:personal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_observe_reports_the_last_reply_instruction_compaction_and_turns() {
    let f = Fixture::new().await;
    f.live(ID, false);
    let (ok, v) = stop(&f, &["--wait"], &[]);
    assert!(ok, "{v}");
    let home = f.paths.account("personal").unwrap();
    let rollout = find_rollout(&home, ID).expect("the saved rollout");
    let line = |at: &str, kind: &str, payload: Value| {
        json!({"timestamp":format!("2026-01-01T00:0{at}.000Z"),"type":kind,"payload":payload})
            .to_string()
            + "\n"
    };
    let text = line(
        "0:01",
        "event_msg",
        json!({"type":"user_message","message":"first ask"}),
    ) + &line(
        "0:02",
        "event_msg",
        json!({"type":"agent_message","message":"first answer"}),
    ) + &line("0:03", "event_msg", json!({"type":"task_complete"}))
        + &line("1:00", "compacted", json!({"message":"summary"}))
        + &line(
            "2:00",
            "event_msg",
            json!({"type":"user_message","message":"second ask"}),
        )
        + &line(
            "2:05",
            "event_msg",
            json!({"type":"agent_message","message":"second answer"}),
        );
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let s = session(&f);
    let (ok, o) = run(
        &f,
        &[
            "observe",
            "--session",
            s["id"].as_str().unwrap(),
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--history",
        ],
        &[],
    );
    assert!(ok, "{o}");
    let o = &o["result"];
    assert_eq!(o["provider"], "codex", "{o}");
    parity_keys::assert_conversation_shape(o);
    let c = &o["conversation"];
    assert_eq!(c["last_reply"]["text"], "second answer", "{c}");
    assert_eq!(c["last_reply_source"], "rollout");
    assert_eq!(c["last_instruction"]["text"], "second ask");
    assert_eq!(c["compacted_at"], 1_767_225_660);
    assert_eq!(c["history_turns"], 10);
    let turns = c["recent_turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "{c}");
    assert_eq!(turns[0]["reply"]["text"], "first answer");
    assert_eq!(turns[0]["ended_at"], 1_767_225_603);
}

/// Contract 1.66.0: `api resume-launch` of a running Codex session is the same
/// `already_running` receipt as for Claude: `phase:"running"`, nothing opened or typed, and
/// the same request again is the same receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_resume_launch_of_a_running_session_is_already_running() {
    use std::process::Stdio;
    let f = Fixture::new().await;
    f.live(ID, false);
    let until = Instant::now() + Duration::from_secs(20);
    let s = loop {
        let s = session(&f);
        if s["availability"] == "running" {
            break s;
        }
        assert!(Instant::now() < until, "{s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let herdr = f.tmp.path().join("herdr");
    fs::write(&herdr, include_str!("../support/fake-herdr.sh")).unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();
    let state = f.tmp.path().join("herdr-state");
    let socket = f.tmp.path().join("host.sock");
    let _host = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let body = json!({"version":1,"request_id":uuid::Uuid::new_v4().to_string(),"session_id":s["id"],
        "expected_generation":s["generation"],"account_id":"codex:personal","name":"again",
        "host":{"session":"fixture","socket":socket},"space":{"kind":"existing","id":"w1"}});
    let start = || {
        let mut child = Command::new(binary())
            .env("TMUX", format!("{},1,0", f.socket))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    f.tmp.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("FAKE_HERDR_DIR", &state)
            .arg("--data-dir")
            .arg(&f.paths.root)
            .args(["api", "resume-launch", "start"])
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
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    };
    let answer = start();
    assert_eq!(answer["phase"], "running", "{answer}");
    assert_eq!(answer["already_running"], true, "{answer}");
    assert_eq!(answer["conversation_id"], ID);
    assert_eq!(answer["request"]["session_id"], s["id"]);
    assert_eq!(start()["request"], answer["request"]);
    let calls = fs::read_to_string(state.join("calls")).unwrap_or_default();
    assert!(
        !calls.contains("tab create") && !calls.contains("pane run"),
        "{calls}"
    );
    assert_eq!(session(&f)["availability"], "running");
}

// ------------------------------------------------ contract 1.68.2: stop --interrupt

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_wait_timeout_names_the_waiting_job_and_a_repeat_returns_it() {
    let f = Fixture::new().await;
    f.live(ID, true);
    f.a.lock().unwrap().hold_busy = true;
    let (ok, v) = stop(
        &f,
        &["--wait"],
        &[("SESSIONSATION_OPERATION_WAIT_MS", "1500")],
    );
    assert!(!ok, "{v}");
    assert_eq!(v["error"]["code"], "OPERATION_TIMEOUT", "{v}");
    assert_eq!(v["error"]["retryable"], true, "{v}");
    let op = &v["error"]["operation"];
    assert_eq!(op["phase"], "waiting_for_idle", "{v}");
    let (ok, again) = stop(&f, &[], &[]);
    assert!(ok, "{again}");
    assert_eq!(again["result"]["id"], op["id"], "{again}");
    assert_eq!(again["result"]["phase"], "waiting_for_idle", "{again}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_interrupt_sends_turn_interrupt_and_completes_at_idle() {
    let f = Fixture::new().await;
    f.live(ID, true);
    let (ok, v) = stop(&f, &["--wait", "--interrupt"], &[]);
    assert!(ok, "{v}");
    assert_eq!(v["result"]["phase"], "complete", "{v}");
    assert_eq!(v["result"]["interrupted_turn"], true, "{v}");
    let calls = f.a.lock().unwrap().calls.clone();
    assert!(
        calls
            .iter()
            .any(|(m, p)| m == "turn/interrupt" && p["turnId"] == "turn1" && p["threadId"] == ID),
        "{calls:?}"
    );
}

/// Contract 1.69.3: queued input refuses a Codex stop at admission (QUEUED_INPUT, not
/// retryable): no turn is interrupted (Codex never starts queued input after an interrupt) and
/// repeated stops add no terminate job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_with_queued_input_is_refused_at_admission_without_a_job() {
    let f = Fixture::new().await;
    f.live(ID, true);
    {
        let mut a = f.a.lock().unwrap();
        a.messaging_profile = true;
        a.native_messages
            .push(json!({"id":"queue-9","clientUserMessageId":"m-9"}));
    }
    for _ in 0..3 {
        let (ok, v) = stop(&f, &["--interrupt"], &[]);
        assert!(!ok, "{v}");
        assert_eq!(v["error"]["code"], "QUEUED_INPUT", "{v}");
        assert_eq!(v["error"]["retryable"], false, "{v}");
        assert!(
            v["error"]["message"].as_str().unwrap().contains("m-9"),
            "{v}"
        );
    }
    assert_eq!(f.invoke(&["moves"]).as_array().unwrap().len(), 0);
    let calls = f.a.lock().unwrap().calls.clone();
    assert!(
        !calls.iter().any(|(m, _)| m == "turn/interrupt"),
        "{calls:?}"
    );
}

/// Contract 1.69.3: `api stop --discard-queued` deletes native queue items no receipt names
/// (reported), so the stop that queued input refused now completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_discard_queued_deletes_native_items_and_stops() {
    let f = Fixture::new().await;
    f.live(ID, true);
    {
        let mut a = f.a.lock().unwrap();
        a.messaging_profile = true;
        a.native_messages
            .push(json!({"id":"queue-9","clientUserMessageId":"m-9"}));
    }
    let (ok, v) = stop(&f, &["--wait", "--interrupt", "--discard-queued"], &[]);
    assert!(ok, "{v}");
    let r = &v["result"];
    assert_eq!(r["phase"], "complete", "{v}");
    assert_eq!(r["interrupted_turn"], true, "{v}");
    assert_eq!(r["discarded_messages"], json!([]), "{v}");
    assert_eq!(
        r["discarded_native_items"],
        json!([{"queued_submission_id":"queue-9",
        "client_user_message_id":"m-9","deleted":true}]),
        "{v}"
    );
    assert!(f.a.lock().unwrap().native_messages.is_empty());
}

/// Contract 1.69.3: a plain Codex stop with queued input still queues its terminate job (Codex
/// drains the queue at idle), and repeated stops return that one job. Only a queue that is
/// already paused behind an interrupted turn refuses it (QUEUED_INPUT, no job).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_plain_stop_queues_once_and_refuses_only_a_paused_queue() {
    let f = Fixture::new().await;
    f.live(ID, true);
    {
        let mut a = f.a.lock().unwrap();
        a.messaging_profile = true;
        a.native_messages
            .push(json!({"id":"queue-9","clientUserMessageId":"m-9"}));
    }
    let mut ids = vec![];
    for _ in 0..3 {
        let (ok, v) = stop(&f, &[], &[]);
        assert!(ok, "{v}");
        ids.push(v["result"]["id"].clone());
    }
    assert!(ids.iter().all(|i| *i == ids[0] && i.is_string()), "{ids:?}");
    assert_eq!(f.invoke(&["moves"]).as_array().unwrap().len(), 1);

    let f = Fixture::new().await;
    f.live(ID, false);
    {
        let mut a = f.a.lock().unwrap();
        a.messaging_profile = true;
        a.native_messages
            .push(json!({"id":"queue-9","clientUserMessageId":"m-9"}));
        a.threads.get_mut(ID).unwrap()["turns"] =
            json!([{"id":"t1","status":"interrupted","items":[],"itemsView":"summary"}]);
    }
    for _ in 0..3 {
        let (ok, v) = stop(&f, &[], &[]);
        assert!(!ok, "{v}");
        assert_eq!(v["error"]["code"], "QUEUED_INPUT", "{v}");
    }
    assert_eq!(f.invoke(&["moves"]).as_array().unwrap().len(), 0);
}

/// Contract 1.71.2 (LEAD p-1125-153509-9488): a plain stop waiting for idle no longer blocks a
/// later `stop --interrupt --discard-queued`: its cancels pass the pending stop job (which has
/// had no side effect), the turn is interrupted and the session stops through that job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_discarding_stop_passes_a_pending_plain_stop_of_the_same_session() {
    let f = Fixture::new().await;
    let pane = f.live(ID, true);
    {
        let mut a = f.a.lock().unwrap();
        a.messaging_profile = true;
        a.hold_busy = true;
    }
    // A kernel-validated launcher record makes the tmux-hosted fixture a messaging target.
    let pid: u32 = tmux(
        &f.socket,
        &["display-message", "-p", "-t", &pane, "#{pane_pid}"],
    )
    .parse()
    .unwrap();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let panes = f.paths.root.join(sessionsation::layout::MANAGED_PANES);
    fs::create_dir_all(&panes).unwrap();
    let home = f.paths.account("personal").unwrap();
    fs::write(
        panes.join("1.json"),
        json!({"pid":pid,"start_time":start,"home":home,"thread":ID}).to_string(),
    )
    .unwrap();
    let s = session(&f);
    let mut ids = vec![];
    for _ in 0..2 {
        let id = uuid::Uuid::new_v4().to_string();
        let req = json!({"api_version":1,"message_id":id,"session_id":s["id"],
            "expected_generation":s["generation"],"mode":"queue","text":"queued fixture input",
            "expires_at":daemon::now()+300});
        let out = super::launch::api(&f, &["send"], Some(&req));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["result"]["state"], "native_accepted", "{v}");
        ids.push(id);
    }
    assert_eq!(f.a.lock().unwrap().native_messages.len(), 2);
    let (ok, plain) = stop(&f, &[], &[]);
    assert!(ok, "{plain}");
    let job = plain["result"]["id"].as_str().unwrap().to_owned();
    f.wait(&job, "waiting_for_idle").await;
    let (ok, v) = stop(&f, &["--interrupt", "--discard-queued", "--wait"], &[]);
    assert!(ok, "{v}");
    let r = &v["result"];
    assert_eq!(r["id"], job.as_str(), "{v}");
    assert_eq!(r["phase"], "complete", "{v}");
    assert_eq!(r["interrupted_turn"], true, "{v}");
    let mut discarded: Vec<String> =
        serde_json::from_value(r["discarded_messages"].clone()).unwrap();
    discarded.sort();
    ids.sort();
    assert_eq!(discarded, ids, "{v}");
    assert_eq!(r["not_discarded"], json!([]), "{v}");
    assert!(f.a.lock().unwrap().native_messages.is_empty());
    for id in &ids {
        let out = super::launch::api(&f, &["receipt", id], None);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["result"]["state"], "cancelled", "{v}");
    }
}

/// Contract 1.71.3: a running Codex row carries top-level `pid`/`process_start` (its attachment)
/// and `caller_binding`: the app-server serving it (the fixture's, in this test process) with
/// CODEX_THREAD_ID; `state --cached` keeps it; a stopped row has none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_rows_bind_callers_to_the_serving_app_server_and_thread_id() {
    let f = Fixture::new().await;
    f.live(ID, false);
    let s = session(&f);
    assert_eq!(s["availability"], "running", "{s}");
    assert_eq!(s["pid"], s["attachment"]["pid"], "{s}");
    assert_eq!(s["process_start"], s["attachment"]["process_start"], "{s}");
    let me = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{me}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let binding = json!({"process":{"pid":me,"process_start":start},
        "env":{"name":"CODEX_THREAD_ID","value":ID}});
    assert_eq!(s["caller_binding"], binding, "{s}");
    assert_eq!(s["caller_binding_reason"], Value::Null);
    let cached = f.invoke(&["api", "state", "--cached"]);
    let row = cached["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["conversation_id"] == ID)
        .unwrap()
        .clone();
    assert_eq!(row["caller_binding"], binding, "{row}");
    let caps = f.invoke(&["api", "capabilities"]);
    assert_eq!(caps["result"]["caller_binding"]["since_contract"], "1.71.3");
    let (ok, v) = stop(&f, &["--wait"], &[]);
    assert!(ok, "{v}");
    let gone = session(&f);
    assert_ne!(gone["availability"], "running", "{gone}");
    assert_eq!(gone["caller_binding"], Value::Null, "{gone}");
    assert_eq!(gone["caller_binding_reason"], "NOT_RUNNING", "{gone}");
    assert_eq!(gone["pid"], Value::Null, "{gone}");
}
