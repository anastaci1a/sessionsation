//! Config rule 2 (SHRINK §3.5): a key a release once wrote is never refused. The table is the
//! live `config.json` key set; each key either is a setting or is in `KNOWN_IGNORED`.
use serde_json::{Value, json};
use sessionsation::config::{Config, KNOWN_IGNORED};

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
fn the_live_key_set_loads_and_ignored_keys_are_reported() {
    let (config, ignored) = Config::parse(live_document()).unwrap();
    config.validate().unwrap();
    assert_eq!(ignored, ["native_sender_registration"]);
    assert_eq!(config.claude_binary, "/usr/local/bin/claude");
    assert_eq!(config.receipt_unknown_horizon_days, 90);
}

#[test]
fn every_live_key_is_a_setting_or_known_ignored() {
    let settings = serde_json::to_value(Config::default()).unwrap();
    for key in LIVE_KEYS {
        let setting = settings.get(*key).is_some();
        assert_ne!(
            setting,
            KNOWN_IGNORED.contains(key),
            "{key} must be exactly one of a setting or KNOWN_IGNORED"
        );
        let mut doc = settings.clone();
        doc[*key] = live_document()[*key].clone();
        assert!(Config::parse(doc).is_ok(), "{key}");
    }
    // An ignored key is accepted alone, and does not come back out.
    let (config, ignored) = Config::parse(json!({"native_sender_registration": true})).unwrap();
    assert_eq!(ignored, ["native_sender_registration"]);
    assert!(
        serde_json::to_value(config)
            .unwrap()
            .get("native_sender_registration")
            .is_none()
    );
}

#[test]
fn an_unknown_key_is_still_refused() {
    assert!(Config::parse(json!({"not_a_setting": 1})).is_err());
    assert!(Config::parse(json!({"idle_poll_seconds": "soon"})).is_err());
}

/// A retired key in `config.json` never reaches stderr: clients parse all of a failing CLI's
/// stderr as one JSON error envelope.
#[test]
fn a_config_with_ignored_keys_keeps_stderr_a_single_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let paths = sessionsation::paths::Paths::new(Some(dir.path().join("root"))).unwrap();
    std::fs::write(
        paths.root.join("config.json"),
        serde_json::to_vec(&live_document()).unwrap(),
    )
    .unwrap();
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
