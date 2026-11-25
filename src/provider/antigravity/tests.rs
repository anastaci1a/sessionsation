use super::*;
use std::os::unix::fs::PermissionsExt;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn child_environment_isolates_home_and_strips_overrides() {
    let config = Config {
        antigravity_binary: "/opt/agy".into(),
        ..Default::default()
    };
    let c = command(&config, Path::new("/srv/accounts/a"));
    assert_eq!(c.get_program(), "/opt/agy");
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
        assert!(envs.contains(&(name.to_string(), None)), "{name}");
    }
    for (k, v) in [
        (HOME_ENV, "/srv/accounts/a"),
        ("HOME", "/srv/accounts/a"),
        ("XDG_CACHE_HOME", "/srv/accounts/a/.cache"),
        ("AGY_CLI_DISABLE_AUTO_UPDATE", "1"),
    ] {
        assert!(envs.contains(&(k.into(), Some(v.into()))), "{k} {envs:?}");
    }
}

#[test]
fn auth_is_token_file_presence_only() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    assert_eq!(auth_status(home).problem(), Some("AUTH_REQUIRED"));
    assert!(
        auth_status(home)
            .gate()
            .unwrap_err()
            .to_string()
            .contains("account login antigravity")
    );
    let dir = state_dir(home);
    fs::create_dir_all(&dir).unwrap();
    let token = dir.join(TOKEN_FILE);
    let before = credentials_stamp(home);
    fs::write(&token, "synthetic").unwrap();
    // Only stat'ed: an unreadable token still counts.
    fs::set_permissions(&token, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(auth_status(home).logged_in);
    assert!(login_completed(home, before));
    assert!(!login_completed(home, credentials_stamp(home)));
    assert_eq!(auth_report(home)["auth_method"], "google-oauth");
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&token, "").unwrap();
    assert!(!auth_status(home).logged_in, "empty token file");
}

#[test]
fn command_lines_identify_agy_sessions() {
    let id = "11111111-1111-4111-8111-111111111111";
    let tui = args(&["/opt/antigravity/install/current/agy", "--conversation", id]);
    assert!(is_agy_arguments(&tui) && !is_print(&tui));
    assert_eq!(conversation_argument(&tui).as_deref(), Some(id));
    let eq = args(&["agy", &format!("--conversation={id}"), YOLO_FLAG]);
    assert_eq!(conversation_argument(&eq).as_deref(), Some(id));
    assert!(is_print(&args(&["agy", "-p=", "--conversation", id])));
    assert!(is_print(&args(&["agy", "--print", "x"])));
    assert_eq!(
        conversation_argument(&args(&["agy", "--conversation", "latest"])),
        None
    );
    assert!(!is_agy_arguments(&args(&["vim", "agy"])));
    assert!(is_agy_arguments(&args(&[
        "/bin/sh",
        "/opt/fake/agy",
        "--conversation",
        id
    ])));
    assert!(!is_agy_arguments(&args(&["/bin/sh", "-c", "agy"])));
}

#[test]
fn launch_arguments_open_the_conversation_and_yolo_skips_permissions() {
    let config = Config::default();
    let id = "11111111-1111-4111-8111-111111111111";
    assert_eq!(
        open_argv(&config, id, LaunchPermissions::Yolo),
        ["agy", "--conversation", id, YOLO_FLAG]
    );
    assert_eq!(
        open_argv(&config, id, LaunchPermissions::Default),
        ["agy", "--conversation", id]
    );
    let a = allocate_args();
    assert!(a.contains(&"--print-timeout".to_string()) && a.last().unwrap() == "-p=");
    let out = b"{\"event\":\"init\",\"conversation_id\":\"11111111-1111-4111-8111-111111111111\",\"init\":{}}\n";
    assert_eq!(allocated_id(out).as_deref(), Some(id));
    assert_eq!(
        allocated_id(b"{\"event\":\"init\",\"conversation_id\":\"\"}"),
        None
    );
}

#[test]
fn queue_mode_follows_settings_argument_then_account_settings() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    assert_eq!(queue_mode(home, None), (Some(QueueMode::Queue), "default"));
    fs::create_dir_all(state_dir(home)).unwrap();
    let settings = state_dir(home).join("settings.json");
    fs::write(&settings, r#"{"queuedMessages":"send-immediately"}"#).unwrap();
    assert_eq!(
        queue_mode(home, None),
        (Some(QueueMode::SendImmediately), "account_settings")
    );
    let inline = args(&["agy", "--settings", r#"{"queuedMessages":"queue"}"#]);
    assert_eq!(
        queue_mode(home, Some(&inline)),
        (Some(QueueMode::Queue), "settings_argument")
    );
    let file = args(&["agy", "--settings=/srv/s.json"]);
    assert_eq!(
        queue_mode(home, Some(&file)).0,
        None,
        "a settings file is not read"
    );
    fs::write(&settings, r#"{"queuedMessages":"later"}"#).unwrap();
    assert_eq!(queue_mode(home, None).0, None);
    fs::write(&settings, "{not json").unwrap();
    assert_eq!(queue_mode(home, None), (None, "settings_unreadable"));
}

#[test]
fn the_conversation_index_gives_identity_directory_and_steps() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    let dir = state_dir(home);
    fs::create_dir_all(dir.join("conversations")).unwrap();
    let db = rusqlite::Connection::open(dir.join("conversation_summaries.db")).unwrap();
    db.execute_batch("CREATE TABLE conversation_summaries (conversation_id text, title text NOT NULL DEFAULT '', step_count integer NOT NULL DEFAULT 0, last_modified_time datetime NOT NULL, workspace_uris text NOT NULL, PRIMARY KEY (conversation_id));").unwrap();
    let a = "11111111-1111-4111-8111-111111111111";
    let empty = "22222222-2222-4222-8222-222222222222";
    let gone = "33333333-3333-4333-8333-333333333333";
    for (id, steps, uri) in [
        (a, 4, "file:///srv/my%20work"),
        (empty, 0, "file:///srv/x"),
        (gone, 2, "file:///srv/y"),
        ("not-a-uuid", 2, "file:///srv/z"),
    ] {
        db.execute(
            "INSERT INTO conversation_summaries (conversation_id, step_count, last_modified_time, workspace_uris) VALUES (?1, ?2, '2026-01-01 00:00:00+00:00', ?3)",
            rusqlite::params![id, steps, format!("[\"{uri}\"]")],
        )
        .unwrap();
    }
    drop(db);
    fs::write(conversation_db(home, a), "x").unwrap();
    fs::write(conversation_db(home, empty), "x").unwrap();
    let listed = transcripts(home);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].session_id, a);
    assert_eq!(listed[0].cwd.as_deref(), Some("/srv/my work"));
    assert!(
        find_transcript(home, empty).is_some(),
        "an empty conversation still exists"
    );
    assert!(find_transcript(home, gone).is_none());
    assert_eq!(observe(home, a, None).availability, "stopped");
    assert_eq!(
        observe(home, a, None).directory.as_deref(),
        Some("/srv/my work")
    );
    assert_eq!(observe(home, gone, None).availability, "stale");
    assert_eq!(uri_path("file:///a%2Fb"), Some("/a/b".into()));
    assert_eq!(uri_path("file://relative"), None);
    assert_eq!(uri_path("https://x"), None);
}
