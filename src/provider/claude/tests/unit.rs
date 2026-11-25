use super::super::*;
fn paths() -> (tempfile::TempDir, Paths) {
    let t = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(t.path().join("root"))).unwrap();
    (t, p)
}
#[test]
fn child_environment_strips_billing_overrides_and_isolates_config() {
    let config = Config {
        claude_binary: "/opt/claude".into(),
        claude_session_home: Some("/srv/claude-home".into()),
        ..Default::default()
    };
    let c = command(&config, Path::new("/tmp/account-home"));
    assert_eq!(c.get_program(), "/opt/claude");
    let envs: Vec<_> = c
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for name in STRIPPED_ENV {
        assert!(
            envs.contains(&(name.to_string(), None)),
            "{name} not removed"
        );
    }
    assert!(!envs.iter().any(|(k, _)| k == "ANTHROPIC_MODEL"));
    assert!(envs.contains(&("CLAUDE_CONFIG_DIR".into(), Some("/tmp/account-home".into()))));
    assert!(envs.contains(&("DISABLE_AUTOUPDATER".into(), Some("1".into()))));
    assert!(envs.contains(&("HOME".into(), Some("/srv/claude-home".into()))));
    assert!(envs.contains(&("XDG_RUNTIME_DIR".into(), None)));
    let inherited = command(&Config::default(), Path::new("/tmp/h"));
    assert!(!inherited.get_envs().any(|(k, _)| k == "HOME"));
}
#[test]
fn auth_gate_requires_subscription_on_first_party() {
    let ok = AuthStatus::parse(br#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"x@example.invalid","orgId":"o"}"#).unwrap();
    assert!(ok.gate().is_ok());
    assert!(!format!("{ok:?}").contains("example.invalid"));
    let out = AuthStatus::parse(br#"{"loggedIn":false}"#).unwrap();
    assert!(
        out.gate()
            .unwrap_err()
            .to_string()
            .starts_with("AUTH_REQUIRED")
    );
    for body in [
        r#"{"loggedIn":true,"authMethod":"apiKey","apiProvider":"firstParty"}"#,
        r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"bedrock"}"#,
    ] {
        let s = AuthStatus::parse(body.as_bytes()).unwrap();
        assert!(
            s.gate()
                .unwrap_err()
                .to_string()
                .starts_with("WRONG_AUTH_MODE")
        );
    }
    assert!(AuthStatus::parse(b"not json").is_err());
}
#[test]
fn adoption_requires_an_existing_unshared_directory_outside_the_root() {
    let (t, p) = paths();
    let state = State::default();
    assert!(validate_adopt(&p, &state, Path::new("relative")).is_err());
    assert!(validate_adopt(&p, &state, &t.path().join("missing")).is_err());
    let file = t.path().join("file");
    fs::write(&file, b"").unwrap();
    assert!(validate_adopt(&p, &state, &file).is_err());
    let inside = p.root.join("accounts/x");
    fs::create_dir_all(&inside).unwrap();
    assert!(
        validate_adopt(&p, &state, &inside)
            .unwrap_err()
            .to_string()
            .contains("INVALID_CONFIG_DIR")
    );
    let external = t.path().join("claude-config");
    fs::create_dir(&external).unwrap();
    assert_eq!(
        validate_adopt(&p, &state, &external).unwrap(),
        fs::canonicalize(&external).unwrap()
    );
    let mut state = State::default();
    let mut a = crate::model::Account::new("other".into(), String::new());
    a.provider = ProviderId::Claude;
    a.config_home = Some(external.clone());
    state.accounts.insert("other".into(), a);
    let nested = external.join("nested");
    fs::create_dir(&nested).unwrap();
    for dir in [&external, &nested] {
        let e = validate_adopt(&p, &state, dir).unwrap_err().to_string();
        assert!(
            e.contains("CONFIG_DIR_IN_USE") && e.contains("claude:other"),
            "{e}"
        );
    }
}
#[test]
fn registry_entries_require_matching_process_start_and_never_read_keys() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let pid = std::process::id();
    let (_, start) = proc_start(pid).unwrap();
    fs::create_dir_all(home.join("sessions")).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    fs::write(
        home.join("sessions").join(format!("{pid}.json")),
        json!({"pid":pid,"sessionId":id,"procStart":(start + 1).to_string(),"status":"busy"})
            .to_string(),
    )
    .unwrap();
    // An unreadable *.key file must never be opened.
    let key = home.join("sessions").join(format!("{pid}.abc.key"));
    fs::write(&key, b"secret").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&key, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        live_sessions(home).unwrap().is_empty(),
        "PID reuse must not count as live"
    );
    let o = observe(home, id, None);
    assert_eq!(o.availability, "stale");
    let dir = home.join("projects/-work");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{id}.jsonl")),
        format!("{}\n", json!({"sessionId":id,"cwd":"/work","timestamp":"2026-01-01T00:00:00Z","message":{"content":"private text"}})),
    )
    .unwrap();
    let o = observe(home, id, None);
    assert_eq!(o.availability, "stopped");
    assert_eq!(o.directory.as_deref(), Some("/work"));
    assert!(!serde_json::to_string(&o).unwrap().contains("private text"));
    let listed = transcripts(home);
    assert_eq!(listed.len(), 1);
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("private text")
    );
    // Matching start but a different config home (this test process has no
    // CLAUDE_CONFIG_DIR) is still not a live session of this home.
    fs::write(
        home.join("sessions").join(format!("{pid}.json")),
        json!({"pid":pid,"sessionId":id,"procStart":start.to_string()}).to_string(),
    )
    .unwrap();
    let expected = std::env::var_os("CLAUDE_CONFIG_DIR").is_none();
    if expected {
        assert!(live_sessions(home).unwrap().is_empty());
    }
}
#[test]
fn continuation_and_fork_markers_use_only_claude_metadata_records() {
    let t = tempfile::tempdir().unwrap();
    let old = "11111111-1111-4111-8111-111111111111";
    let new = "22222222-2222-4222-8222-222222222222";
    let path = t.path().join(format!("{old}.jsonl"));
    let user = json!({"type":"user","sessionId":old,"message":{"content":format!("\"type\":\"continued-in\" {new}")}});
    let marker = json!({"type":"continued-in","timestamp":"2026-10-28T00:00:00Z","sessionId":old,"continuedInSessionId":new});
    fs::write(&path, format!("{user}\n")).unwrap();
    assert_eq!(continued_in(&path), None, "message text is not a marker");
    fs::write(
        &path,
        format!(
            "{user}\n{marker}\n{}\n",
            json!({"type":"last-prompt","sessionId":old})
        ),
    )
    .unwrap();
    assert_eq!(continued_in(&path).as_deref(), Some(new));
    fs::write(&path, format!("{user}\n{marker}\n{user}\n")).unwrap();
    assert_eq!(
        continued_in(&path),
        None,
        "continued again after the hand-off"
    );
    let foreign = json!({"type":"continued-in","sessionId":new,"continuedInSessionId":old});
    fs::write(&path, format!("{foreign}\n")).unwrap();
    assert_eq!(continued_in(&path), None, "marker must name this session");
    let forked = t.path().join(format!("{new}.jsonl"));
    fs::write(
        &forked,
        format!(
            "{}\n",
            json!({"type":"user","sessionId":new,"forkedFrom":{"sessionId":old,"messageUuid":"m"}})
        ),
    )
    .unwrap();
    assert_eq!(forked_from(&forked).as_deref(), Some(old));
    assert_eq!(forked_from(&path), None);
}
#[test]
fn leftover_registry_files_of_killed_processes_never_fail_the_probe() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    fs::create_dir_all(home.join("sessions")).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let dead = child.id();
    child.kill().unwrap();
    child.wait().unwrap();
    let file = home.join("sessions").join(format!("{dead}.json"));
    fs::write(&file, b"{not json").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(home.join("sessions/123.json"), b"{\"pid\":124}").unwrap();
    // Root may still read 0o000 files; either way the probe must succeed.
    assert!(live_sessions(home).unwrap().is_empty());
    assert_ne!(
        observe(home, "11111111-1111-4111-8111-111111111111", None).availability,
        "unknown"
    );
}
#[test]
fn statusline_records_only_limits_and_projection_never_invents_zero() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let now = 1_000_000;
    assert_eq!(project_usage(home, now).2, "missing");
    assert!(project_usage(home, now).0["short"].is_null());
    assert!(!record_statusline(home, &json!({"session_id":"s","version":"2"}), now).unwrap());
    let input = json!({"session_id":"s","version":"2.1.312","model":{"display_name":"secret-model"},
        "workspace":{"current_dir":"/private"},"transcript_path":"/private/t.jsonl",
        "rate_limits":{"five_hour":{"used_percentage":12.5,"resets_at":now+3600},"seven_day":{"used_percentage":40,"resets_at":now+86400}}});
    assert!(record_statusline(home, &input, now).unwrap());
    let file = usage_file(home);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let stored: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let mut keys: Vec<_> = stored.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "claude_version",
            "five_hour",
            "observed_at",
            "session_id",
            "seven_day",
            "source"
        ]
    );
    assert!(!stored.to_string().contains("private"));
    let (usage, fresh, state) = project_usage(home, now + 10);
    assert!(fresh);
    assert_eq!(state, "fresh");
    assert_eq!(usage["short"]["duration_minutes"], 300);
    assert_eq!(usage["short"]["used_percent"], 12.5);
    assert_eq!(usage["weekly"]["duration_minutes"], 10080);
    assert_eq!(usage["source"], USAGE_SOURCE);
    assert!(usage["semantics"].is_string());
    let (_, fresh, state) = project_usage(home, now + USAGE_STALE_SECONDS + 1);
    assert!(!fresh);
    assert_eq!(state, "stale");
    let (_, _, state) = project_usage(home, now + 3601);
    assert_eq!(state, "stale", "a passed reset is stale");
    let partial = json!({"rate_limits":{"five_hour":{"used_percentage":15,"resets_at":now+3600}}});
    // A payload without seven_day carries the recorded window until its own reset,
    // with its own observed_at.
    record_statusline(home, &partial, now + 100).unwrap();
    let (usage, fresh, state) = project_usage(home, now + 100);
    assert_eq!(state, "fresh");
    assert!(fresh);
    assert_eq!(usage["weekly"]["used_percent"], 40.0);
    assert_eq!(usage["weekly"]["observed_at"], now);
    assert_eq!(usage["short"]["observed_at"], now + 100);
    assert_eq!(usage["short"]["used_percent"], 15.0);
    // Past its reset the carried window is dropped: null, never 0.
    let later = now + 86401;
    record_statusline(home, &json!({"rate_limits":{}}), later).unwrap();
    let (usage, _, state) = project_usage(home, later);
    assert_eq!(state, "no_windows");
    assert!(usage["short"].is_null() && usage["weekly"].is_null());
    fs::write(usage_file(home), b"{").unwrap();
    assert_eq!(project_usage(home, now).2, "error");
}
#[test]
fn stale_statusline_replay_never_lowers_usage_within_a_reset_cycle() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let now = 1_000_000;
    let (r5, r7) = (now + 3600, now + 86400);
    let payload = |s: f64, w: f64| {
        json!({"rate_limits":{"five_hour":{"used_percentage":s,"resets_at":r5},
            "seven_day":{"used_percentage":w,"resets_at":r7}}})
    };
    record_statusline(home, &payload(10., 68.), now).unwrap();
    // An idle session replays its last (older, lower) reading.
    let before = fs::read(usage_file(home)).unwrap();
    record_statusline(home, &payload(8., 24.), now + 300).unwrap();
    assert_eq!(
        fs::read(usage_file(home)).unwrap(),
        before,
        "no write at all"
    );
    let (usage, _, _) = project_usage(home, now + 301);
    assert_eq!(usage["weekly"]["used_percent"], 68.0);
    assert_eq!(usage["weekly"]["observed_at"], now);
    // Alternating stale and fresh: the maximum wins, only the grown window refreshes.
    record_statusline(home, &payload(12., 20.), now + 600).unwrap();
    let (usage, _, _) = project_usage(home, now + 601);
    assert_eq!(usage["short"]["used_percent"], 12.0);
    assert_eq!(usage["short"]["observed_at"], now + 600);
    assert_eq!(usage["weekly"]["used_percent"], 68.0);
    assert_eq!(usage["weekly"]["observed_at"], now);
    record_statusline(home, &payload(5., 70.), now + 900).unwrap();
    let (usage, _, _) = project_usage(home, now + 901);
    assert_eq!(usage["short"]["used_percent"], 12.0);
    assert_eq!(usage["short"]["observed_at"], now + 600);
    assert_eq!(usage["weekly"]["used_percent"], 70.0);
    // An equal value is current: it refreshes observed_at.
    record_statusline(home, &payload(12., 70.), now + 1200).unwrap();
    let (usage, _, _) = project_usage(home, now + 1201);
    assert_eq!(usage["short"]["observed_at"], now + 1200);
    assert_eq!(usage["weekly"]["observed_at"], now + 1200);
    // A new resets_at is a new cycle: a lower value replaces.
    let next = json!({"rate_limits":{"five_hour":{"used_percentage":1,"resets_at":r5+18000},
        "seven_day":{"used_percentage":70,"resets_at":r7}}});
    record_statusline(home, &next, now + 4000).unwrap();
    let (usage, _, _) = project_usage(home, now + 4001);
    assert_eq!(usage["short"]["used_percent"], 1.0);
    assert_eq!(usage["short"]["resets_at"], r5 + 18000);
    // Replays of an earlier cycle never replace the current one: an already reset
    // window, and an older (still future) resets_at with higher usage.
    let before = fs::read(usage_file(home)).unwrap();
    for (used, resets) in [(12, r5), (50, r5 + 9000)] {
        let replay = json!({"rate_limits":{"five_hour":{"used_percentage":used,"resets_at":resets},
            "seven_day":{"used_percentage":69,"resets_at":r7}}});
        record_statusline(home, &replay, now + 4100).unwrap();
    }
    assert_eq!(
        fs::read(usage_file(home)).unwrap(),
        before,
        "no write at all"
    );
    let (usage, _, _) = project_usage(home, now + 4101);
    assert_eq!(usage["short"]["used_percent"], 1.0);
    assert_eq!(usage["short"]["resets_at"], r5 + 18000);
}
#[test]
fn recorder_sweeps_only_stale_tmp_files() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join(".sessionsation");
    fs::create_dir_all(&dir).unwrap();
    let (old, young, other) = (
        dir.join(".tmpOLD"),
        dir.join(".tmpYOUNG"),
        dir.join("keep.txt"),
    );
    for p in [&old, &young, &other] {
        fs::write(p, b"x").unwrap();
    }
    let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(aged)
        .unwrap();
    fs::File::options()
        .write(true)
        .open(&other)
        .unwrap()
        .set_modified(aged)
        .unwrap();
    sweep_stale_tmp(&dir, crate::daemon::now());
    assert!(!old.exists() && young.exists() && other.exists());
}
#[test]
fn chained_statusline_skips_the_recorder_itself() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(chained_command(t.path()), None);
    fs::write(
        t.path().join("settings.json"),
        json!({"statusLine":{"command":"echo mine"}}).to_string(),
    )
    .unwrap();
    assert_eq!(chained_command(t.path()).as_deref(), Some("echo mine"));
    fs::write(
        t.path().join("settings.json"),
        json!({"statusLine":{"command":format!("/x/sessionsation {HOOK_MARKER}")}}).to_string(),
    )
    .unwrap();
    assert_eq!(chained_command(t.path()), None);
}
#[test]
fn launch_plans_set_native_identity_name_and_settings() {
    let config = Config::default();
    let settings = launch_settings(
        Path::new("/opt/session sation"),
        LaunchPermissions::Default,
        300,
    );
    assert!(
        serde_json::from_str::<Value>(&settings).unwrap()["skipDangerousModePermissionPrompt"]
            .is_null()
    );
    let v: Value = serde_json::from_str(&settings).unwrap();
    assert!(
        v["statusLine"]["command"]
            .as_str()
            .unwrap()
            .ends_with(HOOK_MARKER)
    );
    assert_eq!(v["statusLine"]["refreshInterval"], 300);
    assert!(
        v["statusLine"]["command"]
            .as_str()
            .unwrap()
            .starts_with('\'')
    );
    let id = "11111111-1111-4111-8111-111111111111";
    assert_eq!(
        new_argv(&config, id, "label", &settings, LaunchPermissions::Default),
        [
            "claude",
            "--session-id",
            id,
            "--name",
            "label",
            "--settings",
            settings.as_str()
        ]
    );
    assert_eq!(
        resume_argv(&config, id, "label", &settings, LaunchPermissions::Default)[1],
        "--resume"
    );
    // yolo: Claude's bypass flag plus the flag-level dialog acceptance.
    let yolo = launch_settings(Path::new("/x"), LaunchPermissions::Yolo, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&yolo).unwrap()["skipDangerousModePermissionPrompt"],
        true
    );
    let options = StartOptions {
        model: Some("claude-fable-5-2".into()),
        effort: Some("high".into()),
        prompt: Some("-brief text".into()),
    };
    let argv = new_argv_with(
        &config,
        id,
        "label",
        &yolo,
        LaunchPermissions::Yolo,
        &options,
    );
    assert_eq!(
        argv[7..],
        [
            "--dangerously-skip-permissions",
            "--model",
            "claude-fable-5-2",
            "--effort",
            "high",
            "--",
            "-brief text"
        ]
    );
    assert_eq!(
        new_argv_with(
            &config,
            id,
            "label",
            &yolo,
            LaunchPermissions::Default,
            &StartOptions::default()
        ),
        new_argv(&config, id, "label", &yolo, LaunchPermissions::Default)
    );
    for argv in [
        new_argv(&config, id, "label", &yolo, LaunchPermissions::Yolo),
        resume_argv(&config, id, "label", &yolo, LaunchPermissions::Yolo),
    ] {
        assert_eq!(argv.last().unwrap(), "--dangerously-skip-permissions");
    }
}

fn sleeper() -> (std::process::Child, u64) {
    let child = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .unwrap();
    let start = proc_start(child.id()).unwrap().1;
    (child, start)
}
fn live(pid: u32, process_start: u64, ms: Option<u64>, spare: bool) -> Live {
    Live {
        pid,
        process_start,
        session_id: "s".into(),
        cwd: None,
        status: None,
        name: None,
        kind: Some("bg".into()),
        job_id: Some("job".into()),
        registry_started_ms: ms,
        spare,
        parked_job_id: None,
        name_source: None,
    }
}
#[test]
fn active_since_never_uses_a_spare_start_and_requires_a_verified_attach_client() {
    let (mut spare, spare_start) = sleeper();
    let (mut client, client_start) = sleeper();
    let l = live(spare.id(), spare_start, Some(1_000), true);
    let attach = |pid: u32, process_start: u64| catalog::Attachment {
        host: "herdr".into(),
        target: "herdr/s/w:p".into(),
        pid,
        process_start,
    };
    // No attachment, the spare itself, or a stale client identity: unknown.
    assert_eq!(active_since(&l, None), None);
    assert_eq!(
        active_since(&l, Some(&attach(spare.id(), spare_start))),
        None
    );
    assert_eq!(
        active_since(&l, Some(&attach(client.id(), client_start + 1))),
        None
    );
    // The verified client's own start, not the spare's registry time.
    let got = active_since(&l, Some(&attach(client.id(), client_start)));
    assert_eq!(got, process_started_at(client_start));
    assert!(got.is_some());
    // A non-spare ignores an implausible (pre-process) registry time.
    let plain = live(client.id(), client_start, Some(1_000), false);
    assert_eq!(active_since(&plain, None), process_started_at(client_start));
    spare.kill().unwrap();
    client.kill().unwrap();
    spare.wait().unwrap();
    client.wait().unwrap();
}
/// Claude Code's `←` gesture, as recorded live: the interactive pane process
/// keeps its registry entry (old session id, operator name, `parkedJobId`)
/// and a `bg` job forked with `--fork-session` holds the conversation, whose
/// old transcript ends in a `continued-in` marker.
#[test]
fn parked_interactive_is_the_terminal_of_its_background_job() {
    let t = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(t.path()).unwrap();
    let spawn = || {
        let child = std::process::Command::new("sleep")
            .arg("600")
            .env("CLAUDE_CONFIG_DIR", &home)
            .spawn()
            .unwrap();
        let start = proc_start(child.id()).unwrap().1;
        (child, start)
    };
    let (mut pane, pane_start) = spawn();
    let (mut job, job_start) = spawn();
    let (pane_pid, job_pid) = (pane.id(), job.id());
    let old = "0946656e-447e-4bcd-a8df-d047792a191a";
    let new = "4ac804e6-c52b-4596-9ca1-603077aebd6e";
    let sessions = home.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let job_sock = home.join("81356.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&job_sock).unwrap();
    let pane_entry = |parked: Option<&str>| {
        let mut v = json!({"pid":pane_pid,"sessionId":old,"procStart":pane_start.to_string(),
            "peerProtocol":1,"kind":"interactive","messagingSocketPath":"/tmp/cc-socks/76131.sock",
            "name":"localica-auditor","nameSource":"user","status":"idle"});
        if let Some(p) = parked {
            v["parkedJobId"] = json!(p);
        }
        fs::write(sessions.join(format!("{pane_pid}.json")), v.to_string()).unwrap();
    };
    pane_entry(Some("4ac804e6"));
    fs::write(
        sessions.join(format!("{job_pid}.json")),
        json!({"pid":job_pid,"sessionId":new,"procStart":job_start.to_string(),"peerProtocol":1,
            "kind":"bg","jobId":"4ac804e6","messagingSocketPath":job_sock,
            "name":"set-effort-level","nameSource":"auto","status":"idle"})
        .to_string(),
    )
    .unwrap();
    // Fork-session lineage: the old transcript hands off to the job's; the
    // job's transcript carries no `forkedFrom` (it is not a side branch).
    let dir = home.join("projects/-work");
    fs::create_dir_all(&dir).unwrap();
    let user = |id: &str| json!({"type":"user","sessionId":id,"cwd":"/work","timestamp":"2026-10-29T16:45:47Z","message":{"content":"/effort"}});
    fs::write(
        dir.join(format!("{old}.jsonl")),
        format!(
            "{}\n{}\n",
            user(old),
            json!({"type":"continued-in","sessionId":old,"continuedInSessionId":new})
        ),
    )
    .unwrap();
    fs::write(dir.join(format!("{new}.jsonl")), format!("{}\n", user(new))).unwrap();
    assert_eq!(
        continued_in(&dir.join(format!("{old}.jsonl"))).as_deref(),
        Some(new)
    );
    assert_eq!(forked_from(&dir.join(format!("{new}.jsonl"))), None);

    let live = live_sessions(&home).unwrap();
    assert_eq!(live.len(), 2, "{live:?}");
    let probe: LiveProbe = Ok(live.clone());
    // The parked record no longer holds the old id; the job holds the new one.
    assert!(holder(&live, old).is_none());
    let bg = holder(&live, new).unwrap();
    assert_eq!(bg.pid, job_pid);
    let front = parked_frontend(&live, bg).unwrap();
    assert_eq!((front.pid, front.process_start), (pane_pid, pane_start));
    assert!(
        parked_frontend(&live, front).is_none(),
        "only a bg job has a frontend"
    );

    let pane_attachment = catalog::Attachment {
        host: "herdr".into(),
        target: "herdr/default/wR:pH".into(),
        pid: pane_pid,
        process_start: pane_start,
    };
    // The entry bound to the job keeps the pane as its terminal, and the
    // generated job title does not replace the operator's name.
    let o = observe_in(&home, new, Some(&pane_attachment), &probe);
    assert_eq!(o.availability, "running");
    assert_eq!(o.attachment.as_ref(), Some(&pane_attachment));
    assert!(!o.attachment_switched);
    assert_eq!(o.pid, Some(job_pid));
    assert_eq!(o.native_kind.as_deref(), Some("bg"));
    assert_eq!(o.job_id.as_deref(), Some("4ac804e6"));
    assert_eq!(o.native_name.as_deref(), Some("localica-auditor"));
    assert!(o.transcript.unwrap().ends_with(format!("{new}.jsonl")));
    // The old id is not held by the parked pane process.
    let o = observe_in(&home, old, Some(&pane_attachment), &probe);
    assert_eq!(o.availability, "stopped");
    assert!(o.attachment.is_none());
    // Exact identity of the frontend, as terminal input admission checks it.
    assert!(attach_client_observed(&pane_attachment, bg));
    assert!(is_parked_frontend(pane_pid, pane_start, "4ac804e6"));
    assert!(is_terminal_client(pane_pid, pane_start, "4ac804e6"));
    assert!(!is_parked_frontend(pane_pid, pane_start + 1, "4ac804e6"));
    assert!(!is_parked_frontend(pane_pid, pane_start, "other123"));
    assert!(!is_parked_frontend(job_pid, job_start, "4ac804e6"));
    // Peer messages (weak steer) go to the job's own socket, not the pane's.
    let endpoint = peer::endpoint(&home, bg).unwrap();
    assert_eq!(endpoint.socket, job_sock);
    assert_eq!(endpoint.pid, job_pid);

    // Unparked (the agents view closed): the pane holds its own session
    // again and is no longer a terminal of the job.
    pane_entry(None);
    let live = live_sessions(&home).unwrap();
    assert_eq!(holder(&live, old).map(|l| l.pid), Some(pane_pid));
    assert!(parked_frontend(&live, holder(&live, new).unwrap()).is_none());
    assert!(!is_parked_frontend(pane_pid, pane_start, "4ac804e6"));
    let o = observe_in(&home, new, Some(&pane_attachment), &Ok(live));
    assert!(o.attachment.is_none());
    assert!(o.attachment_switched);
    assert_eq!(o.native_name.as_deref(), Some("set-effort-level"));

    for c in [&mut pane, &mut job] {
        c.kill().unwrap();
        c.wait().unwrap();
    }
}
#[test]
fn a_job_with_two_parked_frontends_has_none() {
    let mk = |pid: u32, parked: Option<&str>, kind: &str, job: Option<&str>| Live {
        parked_job_id: parked.map(str::to_owned),
        kind: Some(kind.into()),
        job_id: job.map(str::to_owned),
        ..live(pid, 1, None, false)
    };
    let bg = mk(1, None, "bg", Some("job"));
    let one = vec![bg.clone(), mk(2, Some("job"), "interactive", None)];
    assert_eq!(parked_frontend(&one, &bg).map(|l| l.pid), Some(2));
    let two = vec![
        bg.clone(),
        mk(2, Some("job"), "interactive", None),
        mk(3, Some("job"), "interactive", None),
    ];
    assert!(parked_frontend(&two, &bg).is_none());
}
