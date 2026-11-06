use super::*;
use std::sync::Mutex;

fn record(paths: &Paths, session: &Session) -> Record {
    Record {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        kind: "auto_recover".into(),
        trigger: String::new(),
        session_id: session.id.clone(),
        conversation_id: session.conversation_id.clone(),
        provider: "codex".into(),
        account: "a".into(),
        name: session.name.clone(),
        herdr_session: "default".into(),
        recorded_pane: None,
        workspace_label: None,
        directory: None,
        pane: Some("herdr/default/w1:p2".into()),
        pane_reused: Some(true),
        mode: "start_prompt".into(),
        prompt: "go on".into(),
        message_id: uuid::Uuid::new_v4().to_string(),
        prompt_state: "pending".into(),
        prompt_attempts: 0,
        prompt_detail: String::new(),
        resume: "confirmed".into(),
        phase: "resumed".into(),
        detail: String::new(),
        uncertain_from: None,
        generation_after: None,
        created_at: daemon::now(),
        updated_at: daemon::now(),
    }
    .tap(|r| {
        let _ = paths;
        let _ = r;
    })
}
trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}
impl Tap for Record {}

fn fixture() -> (tempfile::TempDir, Paths, Session) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
    let conversation = uuid::Uuid::new_v4().to_string();
    let s = catalog::record_launch(&paths, "a", &conversation, "work", None).unwrap();
    (dir, paths, s)
}

/// Scripted replies for the normal send path; records every body.
struct Script {
    ready_after: Mutex<u32>,
    replies: Mutex<Vec<Result<Value>>>,
    bodies: Mutex<Vec<Value>>,
}
impl Script {
    fn new(mut replies: Vec<Result<Value>>) -> Self {
        replies.reverse();
        Self {
            ready_after: Mutex::new(u32::MAX),
            replies: Mutex::new(replies),
            bodies: Mutex::new(vec![]),
        }
    }
}
impl Sender for Script {
    async fn native_ready(&self, _: &Paths, _: &str) -> bool {
        let mut n = self.ready_after.lock().unwrap();
        if *n == 0 { return true; }
        if *n != u32::MAX { *n -= 1; }
        false
    }
    fn poll(&self) -> Duration { Duration::from_millis(1) }
    fn backoff(&self, _: u32) -> Duration { Duration::from_millis(1) }
    async fn send(&self, _: &Paths, body: Value) -> Result<Value> {
        self.bodies.lock().unwrap().push(body);
        self.replies.lock().unwrap().pop().expect("unexpected send")
    }
}

#[test]
fn the_default_prompt_names_the_restart_time() {
    assert_eq!(chrono_like(0), "1970-01-01 00:00:00 UTC");
    assert_eq!(chrono_like(1_700_000_000), "2023-11-14 22:13:20 UTC");
    assert!(
        default_prompt(1_700_000_000, None)
            .contains("the host restarted at 2023-11-14 22:13:20 UTC")
    );
    assert!(default_prompt(0, None).ends_with("Re-check your state and continue your work."));
    let crash = default_prompt(0, Some("SIGSEGV (segfault)"));
    assert!(crash.contains("crashed (SIGSEGV (segfault))") && !crash.contains("host restarted"));
}

#[test]
fn the_workspace_is_found_by_id_while_its_label_matches_else_by_label() {
    let list = json!({"workspaces":[{"workspace_id":"w1","label":"Other"},{"workspace_id":"w7","label":"Main"}]});
    // Same id, same label: kept.
    assert_eq!(
        pick_workspace(&list, Some("w7"), Some("Main")).as_deref(),
        Some("w7")
    );
    // The id now names a different workspace: find the labelled one.
    assert_eq!(
        pick_workspace(&list, Some("w1"), Some("Main")).as_deref(),
        Some("w7")
    );
    // Label gone: no workspace (the caller creates one).
    assert_eq!(pick_workspace(&list, Some("w1"), Some("Gone")), None);
    // No recorded label: the id alone.
    assert_eq!(
        pick_workspace(&list, Some("w1"), None).as_deref(),
        Some("w1")
    );
    assert_eq!(pick_workspace(&list, None, None), None);
}

#[tokio::test]
async fn the_prompt_is_sent_once_with_one_message_id_and_retried_only_before_admission() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    let script = Script::new(vec![
        Err(anyhow::anyhow!("SESSION_NOT_RUNNING: not yet")),
        Err(anyhow::anyhow!("STALE_TARGET")),
        Ok(json!({"state":"native_accepted"})),
    ]);
    deliver(&paths, &mut r, &script).await.unwrap();
    assert_eq!(r.prompt_state, "delivered");
    assert_eq!(r.prompt_attempts, 3);
    let bodies = script.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    assert!(
        bodies
            .iter()
            .all(|b| b["message_id"] == r.message_id.as_str())
    );
    assert!(
        bodies
            .iter()
            .all(|b| b["text"] == "go on" && b["mode"] == "submit")
    );
    // Delivered is final: another call sends nothing.
    deliver(&paths, &mut r, &Script::new(vec![])).await.unwrap();
    // The state machine also refuses to re-enter after a dispatch.
    r.prompt_state = "dispatching".into();
    assert!(deliver(&paths, &mut r, &Script::new(vec![])).await.is_err());
}

#[tokio::test]
async fn an_ambiguous_send_is_uncertain_and_never_replayed() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    let script = Script::new(vec![Err(anyhow::anyhow!("terminal write timed out"))]);
    let error = deliver(&paths, &mut r, &script).await.unwrap_err();
    assert!(error.to_string().starts_with("PROMPT_DELIVERY_UNCERTAIN"));
    assert_eq!(script.bodies.lock().unwrap().len(), 1);
    assert_eq!(r.prompt_state, "dispatching");
    fail(&paths, &mut r, &error);
    assert_eq!(r.phase, "uncertain");
    assert_eq!(r.uncertain_from.as_deref(), Some("prompting"));
    // Recovery from 'prompting' completes without sending anything.
    r.phase = "prompting".into();
    let silent = Script::new(vec![]);
    drive(&paths, &mut r, &silent).await.unwrap();
    assert_eq!(r.phase, "complete");
    assert!(silent.bodies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_clean_refusal_ends_complete_with_the_prompt_marked_failed() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    let script = Script::new(vec![Err(anyhow::anyhow!("TERMINAL_INPUT_DISABLED: off"))]);
    drive(&paths, &mut r, &script).await.unwrap();
    assert_eq!(r.phase, "complete");
    assert_eq!(r.prompt_state, "failed");
    assert!(r.prompt_detail.contains("TERMINAL_INPUT_DISABLED"));
    assert_eq!(script.bodies.lock().unwrap().len(), 1);
}

#[test]
fn a_failure_before_dispatch_is_failed_and_after_it_uncertain() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    r.phase = "pane_ready".into();
    fail(&paths, &mut r, &anyhow::anyhow!("PANE_NOT_AVAILABLE"));
    assert_eq!(r.phase, "failed");
    r.phase = "resuming".into();
    fail(&paths, &mut r, &anyhow::anyhow!("boom"));
    assert_eq!(r.phase, "uncertain");
    assert_eq!(r.uncertain_from.as_deref(), Some("resuming"));
}

#[test]
fn abandon_needs_the_confirmation_and_a_stuck_operation() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    save(&paths, &mut r).unwrap();
    assert!(
        abandon(&paths, &r.id, "nope")
            .unwrap_err()
            .to_string()
            .contains("CONFIRMATION_MISMATCH")
    );
    assert!(
        abandon(&paths, &r.id, &r.id)
            .unwrap_err()
            .to_string()
            .contains("OPERATION_NOT_STUCK")
    );
    r.phase = "uncertain".into();
    save(&paths, &mut r).unwrap();
    let done = abandon(&paths, &r.id, &r.id).unwrap();
    assert_eq!(done["phase"], "abandoned");
    assert_eq!(done["terminal"], true);
    assert!(exists(&paths, &r.id));
    assert!(
        abandon(&paths, &r.id, &r.id)
            .unwrap_err()
            .to_string()
            .contains("OPERATION_ALREADY_TERMINAL")
    );
}

#[test]
fn explicit_stop_disables_and_suppress_only_pauses() {
    let (_d, paths, s) = fixture();
    with_store(&paths, |store| {
        store.sessions.insert(
            s.id.clone(),
            Setting {
                enabled: true,
                misses: 2,
                ..Default::default()
            },
        );
    })
    .unwrap();
    suppress(&paths, &s.id);
    let x = all(&paths)[&s.id].clone();
    assert!(x.enabled && x.suppressed && x.misses == 0);
    assert_eq!(public(Some(&x))["suppressed"], true);
    explicit_stop(&paths, &s.id);
    let x = all(&paths)[&s.id].clone();
    assert!(x.enabled, "an explicit stop keeps the mode");
    assert!(x.stopped_explicitly_at.is_some());
    assert_eq!(x.last_result.as_ref().unwrap()["code"], "EXPLICIT_STOP");
    retire(&paths, &s.id);
    let x = all(&paths)[&s.id].clone();
    assert!(!x.enabled, "archive and delete disable");
    assert_eq!(
        x.last_result.as_ref().unwrap()["code"],
        "ARCHIVED_OR_DELETED"
    );
    assert_eq!(public(Some(&x))["suppressed"], false);
    // Unknown sessions are ignored and nothing is created.
    explicit_stop(&paths, "no-such-session");
    assert!(!all(&paths).contains_key("no-such-session"));
}

#[test]
fn capability_reasons_follow_the_enable_rules() {
    let live = json!({"provider":"claude","availability":"running","account":"a","hosted":null,"native_kind":"interactive"});
    assert_eq!(capability_reason(&live, false), None);
    let mut stopped = live.clone();
    stopped["availability"] = json!("stopped");
    assert_eq!(
        capability_reason(&stopped, false),
        None,
        "stopped sessions qualify"
    );
    let mut archived = live.clone();
    archived["archived"] = json!(true);
    assert!(capability_reason(&archived, false).is_some());
    // Already enabled: the toggle stays available so it can be turned off.
    assert_eq!(capability_reason(&stopped, true), None);
    let mut hosted = live.clone();
    hosted["hosted"] = json!({"state":"running"});
    assert!(
        capability_reason(&hosted, false)
            .unwrap()
            .starts_with("AUTO_RECOVER_UNSUPPORTED")
    );
    let mut background = live.clone();
    background["native_kind"] = json!("bg");
    assert!(
        capability_reason(&background, false)
            .unwrap()
            .starts_with("AUTO_RECOVER_UNSUPPORTED")
    );
    let mut external = json!({"provider":"codex","availability":"running","account":"external"});
    assert!(
        capability_reason(&external, false)
            .unwrap()
            .starts_with("AUTO_RECOVER_UNSUPPORTED")
    );
    external["account"] = json!("a");
    assert_eq!(capability_reason(&external, false), None);
}

#[test]
fn default_prompt_says_auto_start() {
    assert!(default_prompt(0, None).starts_with("Auto-start:"));
}

#[test]
fn mode_parsing_and_the_enabled_mirror() {
    assert_eq!(Mode::parse("start_prompt").unwrap(), Mode::StartPrompt);
    assert_eq!(Mode::parse("start").unwrap(), Mode::Start);
    assert_eq!(Mode::parse("disabled").unwrap(), Mode::Disabled);
    assert!(Mode::parse("on").is_err());
    assert_eq!(Setting::default().mode(), Mode::Disabled);
    let mut s = Setting::default();
    s.set_mode(Mode::Start);
    assert!(s.enabled);
    s.set_mode(Mode::Disabled);
    assert!(!s.enabled);
}

#[test]
fn old_records_migrate_to_start_prompt_crash_safely_and_idempotently() {
    let (_d, paths, s) = fixture();
    private_dir(&paths.root.join(crate::layout::REGISTRY)).unwrap();
    let old = json!({"schema_version":1,"sessions":{
        s.id.clone(): {"enabled":true,"prompt":"keep me","last_pane":"herdr/x/w1:p1","misses":1},
        "other": {"enabled":false}}});
    fs::write(store_file(&paths), old.to_string()).unwrap();
    // Reads see the migrated mode before anything is rewritten.
    assert_eq!(all(&paths)[&s.id].mode(), Mode::StartPrompt);
    assert_eq!(all(&paths)["other"].mode(), Mode::Disabled);
    migrate(&paths).unwrap();
    let after: Value = serde_json::from_slice(&fs::read(store_file(&paths)).unwrap()).unwrap();
    assert_eq!(after["sessions"][&s.id]["mode"], "start_prompt");
    assert_eq!(after["sessions"][&s.id]["enabled"], true);
    assert_eq!(after["sessions"][&s.id]["prompt"], "keep me");
    assert_eq!(after["sessions"]["other"]["mode"], "disabled");
    assert_eq!(after["schema_version"], 2);
    // Idempotent: a second run leaves the bytes untouched.
    let bytes = fs::read(store_file(&paths)).unwrap();
    migrate(&paths).unwrap();
    migrate(&paths).unwrap();
    assert_eq!(fs::read(store_file(&paths)).unwrap(), bytes);
    // No store: nothing is created.
    let (_d2, p2, _) = fixture();
    migrate(&p2).unwrap();
    assert!(!store_file(&p2).exists());
}

#[tokio::test]
async fn mode_start_sends_no_prompt_and_start_prompt_sends_exactly_once() {
    for (mode, sends) in [("start", 0usize), ("start_prompt", 1)] {
        let (_d, paths, s) = fixture();
        let mut r = record(&paths, &s);
        r.mode = mode.into();
        if mode == "start" {
            r.prompt_state = "skipped".into();
        }
        let script = Script::new(if sends == 1 {
            vec![Ok(json!({"state":"native_accepted"}))]
        } else {
            vec![]
        });
        drive(&paths, &mut r, &script).await.unwrap();
        assert_eq!(r.phase, "complete");
        assert_eq!(script.bodies.lock().unwrap().len(), sends, "{mode}");
        assert!(r.phase != "prompting");
        // Driving again (a recover) never sends a second time.
        drive(&paths, &mut r, &Script::new(vec![])).await.unwrap();
        finish(&paths, &r);
    }
}

#[test]
fn results_use_started_names_and_the_deprecated_projection_keeps_the_old_ones() {
    let (_d, paths, s) = fixture();
    with_store(&paths, |st| {
        let mut x = Setting::default();
        x.set_mode(Mode::Start);
        st.sessions.insert(s.id.clone(), x);
    })
    .unwrap();
    let mut r = record(&paths, &s);
    r.mode = "start".into();
    r.phase = "complete".into();
    r.prompt_state = "skipped".into();
    finish(&paths, &r);
    let x = all(&paths)[&s.id].clone();
    assert_eq!(public_start(Some(&x))["last_result"]["state"], "started");
    assert_eq!(public_start(Some(&x))["mode"], "start");
    // A trigger-named start keeps the legacy projection.
    for (trigger, with, state, legacy) in [
        (
            "boot",
            false,
            "started_after_boot",
            "recovered_without_prompt",
        ),
        (
            "crash",
            true,
            "started_after_crash_with_prompt",
            "recovered",
        ),
    ] {
        let mut t = record(&paths, &s);
        t.trigger = trigger.into();
        t.phase = "complete".into();
        t.prompt_state = if with { "delivered" } else { "skipped" }.into();
        t.mode = if with { "start_prompt" } else { "start" }.into();
        finish(&paths, &t);
        let x = all(&paths)[&s.id].clone();
        assert_eq!(public_start(Some(&x))["last_result"]["state"], state);
        assert_eq!(public(Some(&x))["last_result"]["state"], legacy);
    }
    finish(&paths, &{
        let mut t = r.clone();
        t.trigger = String::new();
        t
    });
    assert_eq!(
        public(Some(&x))["last_result"]["state"],
        "recovered_without_prompt"
    );
    assert_eq!(public(Some(&x))["enabled"], true);
    r.mode = "start_prompt".into();
    r.prompt_state = "delivered".into();
    finish(&paths, &r);
    let x = all(&paths)[&s.id].clone();
    assert_eq!(
        public_start(Some(&x))["last_result"]["state"],
        "started_with_prompt"
    );
    assert_eq!(public(Some(&x))["last_result"]["state"], "recovered");
}

#[test]
fn disable_all_disables_every_enabled_session() {
    let (_d, paths, s) = fixture();
    with_store(&paths, |st| {
        for (id, mode) in [
            (s.id.as_str(), Mode::Start),
            ("b", Mode::StartPrompt),
            ("c", Mode::Disabled),
        ] {
            let mut x = Setting::default();
            x.set_mode(mode);
            st.sessions.insert(id.into(), x);
        }
    })
    .unwrap();
    let out = disable_all(&paths).unwrap();
    assert_eq!(out["disabled_count"], 2);
    assert!(
        all(&paths)
            .values()
            .all(|x| x.mode() == Mode::Disabled && !x.enabled)
    );
    assert_eq!(disable_all(&paths).unwrap()["changed"], false);
}

#[test]
fn kernel_evidence_matches_the_exact_pid_and_ignores_older_lines() {
    let log = "[ 10.000000] Out of memory: Killed process 4242 (claude) total-vm:1kB\n\
               [ 20.000000] claude[77]: segfault at 0 ip 0 sp 0 error 4\n\
               [ 30.000000] traps: node[88] trap invalid opcode ip:0 sp:0 error:0\n";
    // 1/100 s ticks since boot: a process started at 5 s.
    assert!(kernel_evidence(log, 4242, 500).unwrap().contains("OOM"));
    assert!(kernel_evidence(log, 77, 500).unwrap().contains("SIGSEGV"));
    assert!(kernel_evidence(log, 88, 500).unwrap().contains("trap"));
    // Another pid (42 is a prefix of 4242), or a line from before the process started.
    assert_eq!(kernel_evidence(log, 42, 500), None);
    assert_eq!(kernel_evidence(log, 4242, 5000), None);
    assert_eq!(kernel_evidence("", 4242, 0), None);
}

#[test]
fn the_first_boot_record_starts_nothing_and_a_new_boot_makes_modes_due_once() {
    let (_d, paths, s) = fixture();
    with_store(&paths, |st| {
        let mut x = Setting::default();
        x.set_mode(Mode::Start);
        st.sessions.insert(s.id.clone(), x);
    })
    .unwrap();
    detect_boot(&paths, "boot-1").unwrap();
    assert_eq!(
        all(&paths)[&s.id].boot_due,
        None,
        "first record: not a reboot"
    );
    detect_boot(&paths, "boot-1").unwrap();
    assert_eq!(all(&paths)[&s.id].boot_due, None);
    detect_boot(&paths, "boot-2").unwrap();
    assert_eq!(all(&paths)[&s.id].boot_due.as_deref(), Some("boot-2"));
    // Started for that boot: a later pass of the same boot does not make it due again.
    update(&paths, &s.id, |x| {
        x.boot_due = None;
        x.boot_started = Some("boot-2".into());
    });
    detect_boot(&paths, "boot-2").unwrap();
    assert_eq!(all(&paths)[&s.id].boot_due, None);
    // A disabled session is never due.
    update(&paths, &s.id, |x| x.set_mode(Mode::Disabled));
    detect_boot(&paths, "boot-3").unwrap();
    assert_eq!(all(&paths)[&s.id].boot_due, None);
}

fn rejected() -> Value {
    json!({"state":"rejected","reason":"TERMINAL_VIEW_UNDETERMINED","facts":{"written":false,"input_consumed":"unknown"}})
}

#[tokio::test]
async fn a_rejected_receipt_is_retried_with_a_fresh_id_until_accepted() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    let first = r.message_id.clone();
    let script = Script::new(vec![Ok(rejected()), Ok(rejected()), Ok(json!({"state":"native_accepted"}))]);
    deliver(&paths, &mut r, &script).await.unwrap();
    assert_eq!(r.prompt_state, "delivered");
    assert_eq!(r.prompt_attempts, 3);
    let bodies = script.bodies.lock().unwrap().clone();
    assert_ne!(bodies[0]["message_id"], bodies[1]["message_id"]);
    assert_ne!(bodies[1]["message_id"], bodies[2]["message_id"]);
    assert_eq!(bodies[0]["message_id"], first.as_str());
    assert!(r.prompt_detail.contains("rejected/TERMINAL_VIEW_UNDETERMINED"));
    assert!(r.prompt_detail.contains("native_accepted"));
}

#[tokio::test]
async fn an_unconfirmed_or_rejected_only_receipt_is_never_called_delivered_and_uncertain_is_not_retried() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    let script = Script::new(vec![Ok(json!({"state":"delivery_unknown","facts":{"written":false}}))]);
    let e = deliver(&paths, &mut r, &script).await.unwrap_err();
    assert!(e.to_string().starts_with("PROMPT_DELIVERY_UNCERTAIN"));
    assert_eq!(script.bodies.lock().unwrap().len(), 1);
    assert_ne!(r.prompt_state, "delivered");
    // A rejection that typed something (steps journaled) is uncertain too.
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    let mut v = rejected();
    v["terminal_input"] = json!({"steps":[{"step":"clear"}]});
    let script = Script::new(vec![Ok(v)]);
    assert!(deliver(&paths, &mut r, &script).await.is_err());
    assert_eq!(script.bodies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn claude_waits_for_the_native_registry_then_delivers_natively() {
    let (_d, paths, s) = fixture();
    let mut r = record(&paths, &s);
    r.provider = "claude".into();
    let script = Script::new(vec![Ok(json!({"state":"native_accepted"}))]);
    *script.ready_after.lock().unwrap() = 3;
    deliver(&paths, &mut r, &script).await.unwrap();
    assert_eq!(r.prompt_state, "delivered");
    assert_eq!(script.bodies.lock().unwrap()[0]["delivery"], "native");
}

#[test]
fn the_array_epoch_is_the_user_mount_id_and_only_a_new_mount_makes_a_session_due() {
    let info = "20 1 0:5 / /proc rw - proc proc rw\n77 20 0:60 / /mnt/user rw - fuse.shfs shfs rw\n";
    assert_eq!(array_id_in(info, "/mnt/user").as_deref(), Some("77"));
    assert_eq!(array_id_in("20 1 0:5 / /proc rw - proc proc rw\n", "/mnt/user"), None);
    let (_d, paths, s) = fixture();
    with_store(&paths, |st| {
        let mut x = Setting::default();
        x.set_mode(Mode::StartPrompt);
        st.sessions.insert(s.id.clone(), x);
    })
    .unwrap();
    // First pass records only; the same epoch (a supervisor restart) starts nothing.
    detect_array(&paths, "77", false).unwrap();
    detect_array(&paths, "77", false).unwrap();
    assert!(load(&paths).unwrap().sessions[&s.id].array_due.is_none());
    // A new mount on the same boot is an array start: due once.
    detect_array(&paths, "91", false).unwrap();
    assert_eq!(load(&paths).unwrap().sessions[&s.id].array_due.as_deref(), Some("91"));
    // A new boot makes the boot trigger due, not a second array start.
    with_store(&paths, |st| st.sessions.get_mut(&s.id).unwrap().array_due = None).unwrap();
    detect_array(&paths, "5", true).unwrap();
    assert!(load(&paths).unwrap().sessions[&s.id].array_due.is_none());
    // Result names.
    let mut r = record(&paths, &s);
    r.trigger = "array_start".into();
    r.phase = "complete".into();
    r.prompt_state = "delivered".into();
    finish(&paths, &r);
    assert_eq!(all(&paths)[&s.id].last_result.as_ref().unwrap()["state"], "started_after_array_start_with_prompt");
}
