//! Config rule 2 (SHRINK §3.5): a key a release once wrote is never refused. The table is the
//! live `config.json` key set; since 1.70.0 every key is a setting again (`gemini_binary` and
//! `native_sender_registration` are honoured, as in 1.67.1).
use serde_json::{Value, json};
use sessionsation::config::Config;

const LIVE_KEYS: &[&str] = &[
    "active_poll_seconds",
    "claude_binary",
    "codex_binary",
    "gemini_binary",
    "idle_poll_seconds",
    "native_sender_registration",
    "receipt_retention_days",
    "receipt_unknown_horizon_days",
];

fn live_document() -> Value {
    json!({
        "active_poll_seconds": 30,
        "claude_binary": "/usr/local/bin/claude",
        "codex_binary": "/usr/local/bin/codex",
        "gemini_binary": "gemini",
        "idle_poll_seconds": 120,
        "native_sender_registration": false,
        "receipt_retention_days": 14,
        "receipt_unknown_horizon_days": 90,
    })
}

#[test]
fn the_live_key_set_loads_and_every_key_is_honoured() {
    let config: Config = serde_json::from_value(live_document()).unwrap();
    config.validate().unwrap();
    assert_eq!(config.claude_binary, "/usr/local/bin/claude");
    assert_eq!(config.gemini_binary, "gemini");
    assert!(!config.native_sender_registration);
    assert_eq!(config.active_poll_seconds, 30);
    assert_eq!(config.receipt_unknown_horizon_days, 90);
    let on: Config = serde_json::from_value(json!({"native_sender_registration": true})).unwrap();
    assert!(on.native_sender_registration);
    let custom: Config = serde_json::from_value(json!({"gemini_binary": "/opt/gemini"})).unwrap();
    assert_eq!(custom.gemini_binary, "/opt/gemini");
}

#[test]
fn every_live_key_is_a_setting() {
    let settings = serde_json::to_value(Config::default()).unwrap();
    for key in LIVE_KEYS {
        assert!(settings.get(*key).is_some(), "{key} must be a setting");
        let mut doc = settings.clone();
        doc[*key] = live_document()[*key].clone();
        assert!(serde_json::from_value::<Config>(doc).is_ok(), "{key}");
    }
}

#[test]
fn an_unknown_key_is_still_refused() {
    assert!(serde_json::from_value::<Config>(json!({"not_a_setting": 1})).is_err());
    assert!(serde_json::from_value::<Config>(json!({"idle_poll_seconds": "soon"})).is_err());
}

/// The live `config.json` never reaches stderr: clients parse all of a failing CLI's stderr
/// as one JSON error envelope.
#[test]
fn the_live_config_keeps_stderr_a_single_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let paths = sessionsation::paths::Paths::new(Some(dir.path().join("root"))).unwrap();
    std::fs::write(
        paths.root.join("config.json"),
        serde_json::to_vec(&live_document()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        sessionsation::store::load_config(&paths)
            .unwrap()
            .gemini_binary,
        "gemini"
    );
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_sessionsation"))
            .arg("--data-dir")
            .arg(&paths.root)
            .args(args)
            .env_remove("SESSIONSATION_HOME")
            .output()
            .unwrap()
    };
    let ok = run(&["api", "--api-version", "1", "version"]);
    assert!(ok.status.success());
    assert!(
        ok.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let v: Value = serde_json::from_slice(&ok.stdout).unwrap();
    assert!(v["result"]["contract_version"].is_string() || v["contract_version"].is_string());
    let failing = run(&["api", "receipt", "no-such-message"]);
    assert!(!failing.status.success());
    let envelope: Value = serde_json::from_slice(&failing.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr is not one JSON envelope ({e}): {}",
            String::from_utf8_lossy(&failing.stderr)
        )
    });
    assert!(envelope["error"]["code"].is_string(), "{envelope}");
}

#[test]
fn session_tmp_root_is_optional_never_written_unset_and_the_live_key_set_still_loads() {
    // Contract 1.72.1: 1.72.0 refuses unknown keys, so the key is written only when set.
    let default = serde_json::to_value(Config::default()).unwrap();
    assert!(default.get("session_tmp_root").is_none());
    let mut doc = live_document();
    let config: Config = serde_json::from_value(doc.clone()).unwrap();
    assert_eq!(config.session_tmp_root, None);
    for (value, ok) in [
        ("none", true),
        ("", true),
        ("/mnt/disk1/tmp", true),
        ("tmp", false),
    ] {
        doc["session_tmp_root"] = json!(value);
        let config: Config = serde_json::from_value(doc.clone()).unwrap();
        assert_eq!(config.validate().is_ok(), ok, "{value}");
    }
    doc["session_tmp_root"] = Value::Null;
    assert!(
        serde_json::from_value::<Config>(doc)
            .unwrap()
            .session_tmp_root
            .is_none()
    );
}

#[test]
fn a_foreign_owned_session_temp_directory_is_refused() {
    use sessionsation::provider::{ProviderId, session_tmp_env};
    let dir = tempfile::tempdir().unwrap();
    let paths = sessionsation::paths::Paths::new(Some(dir.path().join("root"))).unwrap();
    let env = session_tmp_env(&paths, ProviderId::Codex, "a").unwrap();
    assert_eq!(env.len(), 3, "{env:?}");
    let target = paths.root.join("tmp/codex/a");
    if std::os::unix::fs::chown(&target, Some(12345), None).is_err() {
        return; // not root: cannot make a foreign directory
    }
    let err = session_tmp_env(&paths, ProviderId::Codex, "a").unwrap_err();
    assert!(format!("{err:#}").contains("SESSION_TMP_UNSAFE"), "{err:#}");
}
