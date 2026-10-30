use super::*;
pub(super) fn request(
    session: &Value,
    state: &str,
    stop: Option<&str>,
    plan: Option<Value>,
) -> Value {
    json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"session_id":session["id"],"expected_generation":session["generation"],"mode":"resume","text":"fixture recovery input","expires_at":daemon::now()+300,
        "recovery":{"expected_state":state,"expected_previous_turn_id":"turn","expected_stop_id":stop,"launch":plan}})
}
pub(super) async fn receipt(f: &Fixture, id: &str) -> Value {
    let until = Instant::now() + Duration::from_secs(40);
    loop {
        let out = launch::api(f, &["receipt", id], None);
        if !out.status.success() {
            let err: Value = serde_json::from_slice(&out.stderr).unwrap();
            assert!(Instant::now() < until, "{err}");
            if err["error"]["code"] == "ADAPTER_BUSY" {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            panic!("{err}");
        }
        let r: Value = serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone();
        let phase = r["launch"]["phase"].as_str().unwrap_or("");
        if matches!(phase, "complete" | "failed" | "uncertain") {
            return r;
        }
        assert!(Instant::now() < until, "recovery stuck: {r}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
pub(super) fn saved(f: &Fixture, status: &str) -> Value {
    f.a.lock().unwrap().messaging_profile = true;
    let home = f.paths.account("personal").unwrap();
    let p = history(&home, f.tmp.path(), ID);
    let mut t = thread_value(&p, ID, "notLoaded");
    t["turns"] = json!([{"id":"turn","status":status}]);
    f.a.lock().unwrap().threads.insert(ID.into(), t);
    let all = f.invoke(&["api", "state"]);
    all["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == ID)
        .unwrap()
        .clone()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_exited_tmux_recovery_preserves_identity_context_and_sends_once() {
    for state in ["stopped", "interrupted", "errored"] {
        let f = Fixture::new().await;
        tmux(
            &f.socket,
            &["new-session", "-d", "-s", "unrelated", "sleep 300"],
        );
        let s = saved(
            &f,
            match state {
                "interrupted" => "interrupted",
                "errored" => "failed",
                _ => "completed",
            },
        );
        let stop = if state == "stopped" {
            Some("explicit-stop")
        } else {
            None
        };
        if let Some(stop) = stop {
            fs::create_dir_all(f.paths.root.join(sessionsation::layout::CONVERSATIONS)).unwrap();
            fs::write(
                f.paths
                    .root
                    .join(sessionsation::layout::CONVERSATIONS)
                    .join(format!("{ID}.json")),
                json!({"stop_intent":stop,"owner":f.paths.account("personal").unwrap()})
                    .to_string(),
            )
            .unwrap();
        }
        let input = json!({"session_id":s["id"],"expected_generation":s["generation"],"host":{"kind":"tmux","socket":f.socket,"session":"recovered"}});
        let plan = launch::api(&f, &["recovery-plan"], Some(&input));
        assert!(
            plan.status.success(),
            "{}",
            String::from_utf8_lossy(&plan.stderr)
        );
        let plan: Value = serde_json::from_slice::<Value>(&plan.stdout).unwrap()["result"].clone();
        let req = request(&s, state, stop, Some(plan));
        let submitted = launch::api(&f, &["send"], Some(&req));
        assert!(
            submitted.status.success(),
            "{}",
            String::from_utf8_lossy(&submitted.stderr)
        );
        // A receipt reader may still own the ledger as the agent finishes
        // launching. Contention must delay the one delivery, not reject it.
        use fs2::FileExt;
        let observer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.paths.root.join("messaging/adapter.lock"))
            .unwrap();
        observer.lock_exclusive().unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let journal: Value = serde_json::from_slice(
                &fs::read(
                    f.paths
                        .root
                        .join(sessionsation::layout::MESSAGE_RECOVERIES)
                        .join(format!("{}.json", req["message_id"].as_str().unwrap())),
                )
                .unwrap(),
            )
            .unwrap();
            if journal["phase"] == "launch_confirmed" {
                break;
            }
            assert!(
                !matches!(journal["phase"].as_str(), Some("failed" | "uncertain")),
                "{journal}"
            );
            assert!(
                Instant::now() < until,
                "launch did not reach ledger contention: {journal}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(observer);
        let done = receipt(&f, req["message_id"].as_str().unwrap()).await;
        assert_eq!(done["launch"]["phase"], "complete", "{done}");
        assert_eq!(done["facts"]["accepted_native"], true, "{done}");
        assert_eq!(done["recovery_start"]["state"], "confirmed", "{done}");
        assert_eq!(
            done["facts"]["input_recorded"], false,
            "a start acknowledgement is not recorded input"
        );
        assert_eq!(done["provider_conversation_id"], ID);
        assert_eq!(done["session_id"], s["id"]);
        let retry = launch::api(&f, &["send"], Some(&req));
        assert!(
            retry.status.success(),
            "{}",
            String::from_utf8_lossy(&retry.stderr)
        );
        let state = f.invoke(&["api", "state"]);
        let now = state["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == s["id"])
            .unwrap();
        assert_eq!(now["conversation_id"], ID);
        assert_eq!(now["account_id"], "codex:personal");
        assert_eq!(now["directory"], f.tmp.path().to_str().unwrap());
        let calls = f.a.lock().unwrap().calls.clone();
        assert_eq!(
            calls
                .iter()
                .filter(|(m, _)| m == "thread/queue/start")
                .count(),
            1
        );
        assert!(f.a.lock().unwrap().native_messages.is_empty());
        assert_eq!(
            calls
                .iter()
                .filter(|(m, _)| m == "thread/queue/add")
                .count(),
            1
        );
        assert_eq!(
            calls.iter().filter(|(m, _)| m == "thread/resume").count(),
            1
        );
        assert!(
            !calls
                .iter()
                .any(|(m, _)| matches!(m.as_str(), "thread/start" | "turn/start" | "turn/steer"))
        );
        let cmd =
            fs::read_to_string(f.paths.account("personal").unwrap().join("launch-args")).unwrap();
        assert!(cmd.contains(ID));
        assert!(cmd.contains("--remote"));
        assert!(cmd.contains("app-server-control.sock"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_messages_observers_and_missing_recovery_plan_never_launch_exited_agents() {
    let f = Fixture::new().await;
    let s = saved(&f, "completed");
    for mode in ["submit", "queue", "resume"] {
        let mut r = request(&s, "stopped", None, None);
        r["mode"] = json!(mode);
        if mode != "resume" {
            r.as_object_mut().unwrap().remove("recovery");
        }
        let out = launch::api(&f, &["send"], Some(&r));
        assert!(!out.status.success());
        if mode == "resume" {
            let e: Value = serde_json::from_slice(&out.stderr).unwrap();
            assert_eq!(
                e["error"]["code"], "RECOVERY_EXITED_PROCESS_REQUIRES_VALIDATED_LAUNCH_PLAN",
                "{e}"
            );
        }
    }
    let _ = launch::api(
        &f,
        &[
            "observe",
            "--session",
            s["id"].as_str().unwrap(),
            "--expected-generation",
            s["generation"].as_str().unwrap(),
        ],
        None,
    );
    let _ = launch::api(
        &f,
        &["observations"],
        Some(
            &json!({"api_version":1,"sessions":[{"session_id":s["id"],"expected_generation":s["generation"]}]}),
        ),
    );
    f.invoke(&["api", "state"]);
    assert!(!f.a.lock().unwrap().calls.iter().any(|(m, _)| matches!(
        m.as_str(),
        "thread/resume" | "thread/start" | "turn/start" | "turn/steer" | "thread/queue/add"
    )));
    assert!(
        !f.paths
            .account("personal")
            .unwrap()
            .join("launch-args")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_launch_material_and_queued_work_refuse_before_creating_a_process() {
    let f = Fixture::new().await;
    tmux(
        &f.socket,
        &["new-session", "-d", "-s", "unrelated", "sleep 300"],
    );
    let s = saved(&f, "completed");
    let input = json!({"session_id":s["id"],"expected_generation":s["generation"],"host":{"kind":"tmux","socket":f.socket,"session":"recovered"}});
    let out = launch::api(&f, &["recovery-plan"], Some(&input));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan = serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone();
    for (field, code) in [
        (
            "executable_sha256",
            "RECOVERY_EXECUTABLE_CHANGED_REVALIDATE",
        ),
        ("history_sha256", "RECOVERY_HISTORY_CHANGED_REVALIDATE"),
        (
            "native_endpoint_generation",
            "RECOVERY_NATIVE_OWNER_CHANGED_REVALIDATE",
        ),
    ] {
        let mut wrong = plan.clone();
        wrong[field] = json!("0".repeat(64));
        let r = request(&s, "stopped", None, Some(wrong));
        let out = launch::api(&f, &["send"], Some(&r));
        assert!(!out.status.success());
        let e: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(e["error"]["code"], code, "{e}");
    }
    f.a.lock()
        .unwrap()
        .native_messages
        .push(json!({"id":"older","clientUserMessageId":uuid::Uuid::new_v4().to_string()}));
    let out = launch::api(
        &f,
        &["send"],
        Some(&request(&s, "stopped", None, Some(plan))),
    );
    assert!(!out.status.success());
    let e: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(
        e["error"]["code"], "RECOVERY_EXISTING_QUEUED_WORK_RESOLVE_BEFORE_RESUME",
        "{e}"
    );
    f.a.lock().unwrap().native_messages.clear();
    let history = find_rollout(&f.paths.account("personal").unwrap(), ID).unwrap();
    let text = fs::read_to_string(&history).unwrap();
    let text = text
        .lines()
        .filter(|l| serde_json::from_str::<Value>(l).unwrap()["type"] != "turn_context")
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(history, text).unwrap();
    let out = launch::api(&f, &["recovery-plan"], Some(&input));
    assert!(!out.status.success());
    let e: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(
        e["error"]["code"], "RECOVERY_PERMISSIONS_MISSING_RECORD_EXPLICIT_LAUNCH_POLICY_FIRST",
        "{e}"
    );
    assert_eq!(
        tmux(&f.socket, &["list-sessions", "-F", "#{session_name}"]),
        "unrelated"
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| matches!(m.as_str(), "thread/resume" | "thread/queue/add"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retained_live_recovery_is_preconditioned_and_lost_ack_is_never_replayed() {
    for (state, lost) in [
        ("stopped", false),
        ("interrupted", false),
        ("errored", true),
    ] {
        let f = Fixture::new().await;
        let pane = f.live(ID, false);
        f.a.lock().unwrap().messaging_profile = true;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let snapshot = f.invoke(&["api", "state"]);
        let s = snapshot["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["conversation_id"] == ID)
            .unwrap()
            .clone();
        let a = &s["attachment"];
        let home = f.paths.account("personal").unwrap();
        fs::create_dir_all(f.paths.root.join(sessionsation::layout::MANAGED_PANES)).unwrap();
        fs::write(
            f.paths
                .root
                .join(sessionsation::layout::MANAGED_PANES)
                .join(format!("{}.json", pane.trim_start_matches('%'))),
            json!({"pid":a["pid"],"start_time":a["process_start"],"home":home,"thread":ID})
                .to_string(),
        )
        .unwrap();
        let stop = if state == "stopped" {
            Some("stop-token")
        } else {
            None
        };
        if let Some(stop) = stop {
            fs::create_dir_all(f.paths.root.join(sessionsation::layout::CONVERSATIONS)).unwrap();
            fs::write(
                f.paths
                    .root
                    .join(sessionsation::layout::CONVERSATIONS)
                    .join(format!("{ID}.json")),
                json!({"stop_intent":stop,"owner":home}).to_string(),
            )
            .unwrap();
        }
        {
            let mut server = f.a.lock().unwrap();
            server.lose_queue_reply = lost;
            server.threads.get_mut(ID).unwrap()["turns"] = json!([{"id":"turn","status":match state{"interrupted"=>"interrupted","errored"=>"failed",_=>"completed"}}]);
        }
        let observed = launch::api(
            &f,
            &[
                "observe",
                "--session",
                s["id"].as_str().unwrap(),
                "--expected-generation",
                s["generation"].as_str().unwrap(),
            ],
            None,
        );
        assert!(
            observed.status.success(),
            "{}",
            String::from_utf8_lossy(&observed.stderr)
        );
        let observed = serde_json::from_slice::<Value>(&observed.stdout).unwrap()["result"].clone();
        assert_eq!(
            observed["actions"]["resume"]["available"], true,
            "{observed}"
        );
        let mut r = request(&s, state, stop, None);
        r["recovery"]["expected_runtime_generation"] = observed["runtime_generation"].clone();
        let mut wrong = r.clone();
        wrong["message_id"] = json!(uuid::Uuid::new_v4().to_string());
        wrong["recovery"]["expected_previous_turn_id"] = json!("changed");
        let rejected = launch::api(&f, &["send"], Some(&wrong));
        assert!(!rejected.status.success());
        let out = launch::api(&f, &["send"], Some(&r));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let done = receipt(&f, r["message_id"].as_str().unwrap()).await;
        assert_eq!(done["launch"]["required"], false, "{done}");
        assert_eq!(done["launch"]["phase"], "complete", "{done}");
        for _ in 0..3 {
            assert!(launch::api(&f, &["send"], Some(&r)).status.success());
        }
        let calls = f.a.lock().unwrap().calls.clone();
        assert_eq!(
            calls
                .iter()
                .filter(|(m, _)| m == "thread/queue/add")
                .count(),
            1
        );
        assert!(!calls.iter().any(|(m, _)| matches!(
            m.as_str(),
            "thread/resume" | "thread/start" | "turn/start" | "turn/steer"
        )));
        assert_eq!(
            tmux(
                &f.socket,
                &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
            )
            .parse::<u64>()
            .unwrap(),
            a["pid"].as_u64().unwrap()
        );
        let mut changed = r.clone();
        changed["text"] = json!("different");
        let out = launch::api(&f, &["send"], Some(&changed));
        assert!(!out.status.success());
        let err: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["code"], "MESSAGE_ID_CONFLICT");
    }
}
