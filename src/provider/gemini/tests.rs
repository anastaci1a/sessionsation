use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn child_environment_isolates_home_and_strips_backend_overrides() {
    let config = Config {
        gemini_binary: "/opt/gemini".into(),
        ..Default::default()
    };
    let c = command(&config, Path::new("/tmp/account-home"));
    assert_eq!(c.get_program(), "/opt/gemini");
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
    assert!(
        envs.contains(&("GEMINI_CLI_HOME".into(), Some("/tmp/account-home".into()))),
        "{envs:?}"
    );
    assert!(envs.contains(&("GEMINI_CLI_NO_RELAUNCH".into(), Some("1".into()))));
    assert!(envs.contains(&("GEMINI_FORCE_FILE_STORAGE".into(), Some("true".into()))));
    assert!(!envs.iter().any(|(k, _)| k == "NO_BROWSER"));
    let login = login_command(&config, Path::new("/tmp/account-home"));
    assert!(
        login
            .get_envs()
            .any(|(k, v)| k == "NO_BROWSER" && v.is_some_and(|v| v == "true"))
    );
}

#[test]
fn api_key_accounts_get_their_key_by_environment_only_and_google_accounts_never() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    let config = Config {
        gemini_binary: "/opt/gemini".into(),
        ..Default::default()
    };
    let env = |c: &std::process::Command, name: &str| {
        c.get_envs()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    };
    seed_settings(home).unwrap();
    auth_mode::store_key(home, &auth_mode::ApiKey::new("AIzaFAKE-env").unwrap()).unwrap();
    let c = command(&config, home);
    assert_eq!(
        env(&c, "GEMINI_API_KEY"),
        Some(None),
        "google mode strips it"
    );
    assert_eq!(env(&c, auth_mode::WRAPPER_ENV), Some(None));
    assert_eq!(auth_status(home).mode, auth_mode::Mode::Google);
    auth_mode::set_mode(home, auth_mode::Mode::ApiKey).unwrap();
    let c = command(&config, home);
    assert_eq!(env(&c, "GEMINI_API_KEY"), Some(Some("AIzaFAKE-env".into())));
    assert_eq!(
        env(&c, auth_mode::WRAPPER_ENV),
        Some(Some("AIzaFAKE-env".into()))
    );
    assert!(c.get_args().all(|a| a != "AIzaFAKE-env"));
    let s = auth_status(home);
    assert!(s.logged_in && s.gate().is_ok());
    assert_eq!(s.mode()["auth_method"], "gemini-api-key");
    assert_eq!(s.mode()["auth_mode"], "api_key");
    assert!(!auth_report(home).to_string().contains("AIzaFAKE"));
    logout(home).unwrap();
    let s = auth_status(home);
    assert!(!s.logged_in);
    assert!(s.gate().unwrap_err().to_string().contains("--mode api-key"));
    assert_eq!(env(&command(&config, home), "GEMINI_API_KEY"), Some(None));
}

#[test]
fn settings_are_seeded_without_clobbering_other_keys() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    seed_settings(home).unwrap();
    let path = home.join(".gemini/settings.json");
    let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["general"]["enableAutoUpdate"], false);
    assert_eq!(v["general"]["enableAutoUpdateNotification"], false);
    assert_eq!(v["privacy"]["usageStatisticsEnabled"], false);
    assert_eq!(v["general"]["sessionRetention"]["enabled"], false);
    assert_eq!(v["security"]["auth"]["selectedType"], "oauth-personal");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::write(
        &path,
        json!({"theme":"dark","general":{"enableAutoUpdate":true,"vimMode":true},
            "security":{"auth":{"selectedType":"gemini-api-key"}}})
        .to_string(),
    )
    .unwrap();
    seed_settings(home).unwrap();
    let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["general"]["vimMode"], true);
    assert_eq!(
        v["general"]["enableAutoUpdate"], false,
        "auto-update is always disabled"
    );
    assert_eq!(
        v["security"]["auth"]["selectedType"], "gemini-api-key",
        "an operator's own auth selection is kept"
    );
    // Invalid settings are refused and left alone.
    fs::write(&path, "{not json").unwrap();
    assert!(
        seed_settings(home)
            .unwrap_err()
            .to_string()
            .contains("UNSAFE_SETTINGS")
    );
    assert_eq!(fs::read(&path).unwrap(), b"{not json");
}

#[test]
fn auth_needs_credential_file_and_an_active_account_and_never_reads_credentials() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    assert!(!auth_status(home).logged_in);
    assert_eq!(auth_status(home).problem(), Some("AUTH_REQUIRED"));
    let dir = home.join(".gemini");
    fs::create_dir_all(&dir).unwrap();
    let creds = dir.join("oauth_creds.json");
    fs::write(&creds, "synthetic").unwrap();
    // The credential file is only stat'ed: an unreadable one still counts.
    fs::set_permissions(&creds, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(!auth_status(home).logged_in, "no active account recorded");
    fs::write(
        dir.join("google_accounts.json"),
        json!({"active":null,"old":[]}).to_string(),
    )
    .unwrap();
    assert!(!auth_status(home).logged_in);
    fs::write(
        dir.join("google_accounts.json"),
        json!({"active":"fixture@example.invalid","old":[]}).to_string(),
    )
    .unwrap();
    let s = auth_status(home);
    assert!(s.logged_in && s.gate().is_ok());
    assert!(!auth_report(home).to_string().contains("example.invalid"));
    fs::set_permissions(&creds, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&creds, "").unwrap();
    assert!(!auth_status(home).logged_in, "empty credential file");
}

#[test]
fn login_completes_only_when_credentials_appear_or_change() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    let dir = home.join(".gemini");
    fs::create_dir_all(&dir).unwrap();
    let before = credentials_stamp(home);
    assert!(!login_completed(home, before));
    fs::write(dir.join("oauth_creds.json"), "{}").unwrap();
    fs::write(
        dir.join("google_accounts.json"),
        json!({"active":"a@example.invalid"}).to_string(),
    )
    .unwrap();
    assert!(
        login_completed(home, before),
        "new credentials complete the login"
    );
    let existing = credentials_stamp(home);
    assert!(
        !login_completed(home, existing),
        "an already-logged-in account is not complete until relogin rewrites it"
    );
    fs::write(dir.join("oauth_creds.json"), "{\"refreshed\":1}").unwrap();
    assert!(login_completed(home, existing));
}

#[test]
fn chat_headers_and_project_registry_give_identity_and_directory() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    let id = "11111111-1111-4111-8111-111111111111";
    let slug = home.join(".gemini/tmp/work/chats");
    fs::create_dir_all(&slug).unwrap();
    let file = slug.join("session-2026-01-01T00-00-11111111.jsonl");
    fs::write(
        &file,
        format!("{}\n{}\n", json!({"sessionId":id,"projectHash":"h","startTime":"2026-01-01T00:00:00Z","lastUpdated":"2026-01-01T00:00:00Z"}),
            json!({"type":"user","content":"private text"})),
    )
    .unwrap();
    // Wrong file suffix and non-UUID ids are not sessions.
    fs::write(
        slug.join("session-2026-01-01T00-00-deadbeef.jsonl"),
        format!("{}\n", json!({"sessionId":id,"projectHash":"h"})),
    )
    .unwrap();
    fs::write(
        slug.join("session-2026-01-01T00-00-notauuid.jsonl"),
        format!("{}\n", json!({"sessionId":"notauuid","projectHash":"h"})),
    )
    .unwrap();
    assert_eq!(find_transcript(home, id).as_deref(), Some(file.as_path()));
    let listed = transcripts(home);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].cwd, None, "no registry entry yet");
    fs::write(
        home.join(".gemini/projects.json"),
        json!({"projects":{"/srv/work":"work","relative":"x"}}).to_string(),
    )
    .unwrap();
    let listed = transcripts(home);
    assert_eq!(listed[0].cwd.as_deref(), Some("/srv/work"));
    assert_eq!(
        listed[0].started_at.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("private text")
    );
    let o = observe(home, id, None);
    assert_eq!(o.availability, "stopped");
    assert_eq!(o.directory.as_deref(), Some("/srv/work"));
    assert_eq!(
        observe(home, "22222222-2222-4222-8222-222222222222", None).availability,
        "stale"
    );
    // A symlinked chat file is never followed.
    let outside = d.path().join("outside.jsonl");
    fs::write(&outside, "{}").unwrap();
    std::os::unix::fs::symlink(&outside, slug.join("session-x-33333333.jsonl")).unwrap();
    assert_eq!(transcripts(home).len(), 1);
}

#[test]
fn command_lines_identify_gemini_sessions_only() {
    let id = "11111111-1111-4111-8111-111111111111";
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let node = args(&["node", "/opt/gemini/bundle/gemini.js", "--session-id", id]);
    assert!(is_gemini_arguments(&node));
    assert_eq!(session_argument(&node).as_deref(), Some(id));
    let resumed = args(&["/usr/bin/gemini", "-r", id]);
    assert!(is_gemini_arguments(&resumed));
    assert_eq!(session_argument(&resumed).as_deref(), Some(id));
    assert_eq!(session_argument(&args(&["gemini", "-r", "latest"])), None);
    assert!(!is_gemini_arguments(&args(&[
        "vim",
        "notes",
        "--session-id",
        id
    ])));
    assert!(!is_gemini_arguments(&args(&["sh", "-c", "x", "gemini.js"])));
}
#[test]
fn live_identity_requires_command_line_home_and_exact_start_time() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let other = d.path().join("other");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&other).unwrap();
    let script = d.path().join("gemini.js");
    fs::write(&script, "#!/bin/sh\nread _\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let spawn = |home: &Path, flag: &str, id: &str| {
        std::process::Command::new(&script)
            .args([flag, id])
            .env("GEMINI_CLI_HOME", home)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut child = spawn(&home, "--session-id", id);
    let mut stranger = spawn(&other, "-r", "22222222-2222-4222-8222-222222222222");
    let pid = child.id();
    // The shell blocks in the `read` builtin and never execs, so its command line stays the
    // script's; wait only for the fork-to-exec window.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let start = loop {
        if let Some((_, start)) = proc_start(pid)
            && arguments(pid).is_some_and(|a| is_gemini_arguments(&a))
        {
            break start;
        }
        assert!(std::time::Instant::now() < deadline, "script did not start");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let live = live_sessions(&home).unwrap();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!((live[0].pid, live[0].session_id.as_str()), (pid, id));
    assert!(
        live_sessions(&other)
            .unwrap()
            .iter()
            .all(|l| l.session_id != id),
        "a process of another home never holds this account's session"
    );
    assert!(env_in_use(&[home.as_path()]) && !env_in_use(&[d.path()]));
    let attachment = |pid, process_start| catalog::Attachment {
        host: "herdr".into(),
        target: "herdr/test/w1:p1".into(),
        pid,
        process_start,
    };
    let session = |id: &str, a: catalog::Attachment| catalog::Session {
        id: "catalog-id".into(),
        name: "n".into(),
        provider: "gemini".into(),
        account: "a".into(),
        conversation_id: id.into(),
        revision: 1,
        attachment: Some(a),
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    };
    assert!(launch_observed(&session(id, attachment(pid, start))));
    assert!(
        !launch_observed(&session(id, attachment(pid, start + 1))),
        "a recycled PID with another start time is not the session"
    );
    assert!(
        !launch_observed(&session(
            "33333333-3333-4333-8333-333333333333",
            attachment(pid, start)
        )),
        "the process runs another native session"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = stranger.kill();
    let _ = stranger.wait();
    assert!(!launch_observed(&session(id, attachment(pid, start))));
    assert!(live_sessions(&home).unwrap().is_empty());
}
