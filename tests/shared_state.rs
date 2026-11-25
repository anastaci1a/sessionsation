//! `sessionsation shared status|enable` through the real binary on disposable roots (contract 1.39.0).
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::Value;
use sessionsation::{
    model::{Account, State},
    paths::Paths,
    provider::ProviderId,
    store::Store,
};
use std::{
    path::PathBuf,
    process::{Command, Output},
};

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
}
fn fixture(accounts: &[&str]) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let mut store = Store::open(&paths).unwrap();
    let mut state = State::default();
    for name in accounts {
        sessionsation::provider::claude::create_home(&paths, name).unwrap();
        state.insert_account(Account::with_provider(
            ProviderId::Claude,
            (*name).into(),
            String::new(),
        ));
    }
    store.save(&state, 1, "test", "fixture").unwrap();
    Fx {
        root: paths.root.clone(),
        _dir: dir,
    }
}
impl Fx {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sessionsation"))
            .arg("--data-dir")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn home(&self, name: &str) -> PathBuf {
        self.root.join("accounts/claude").join(name)
    }
}
#[test]
fn capabilities_advertise_shared_claude_state() {
    let f = fixture(&[]);
    let caps = f.json(&["api", "capabilities"]);
    assert_eq!(caps["result"]["contract_version"], "1.69.3");
    let shared = &caps["result"]["shared_claude_state"];
    assert_eq!(shared["version"], 1);
    assert_eq!(
        shared["names"],
        serde_json::json!([
            "projects",
            "file-history",
            "todos",
            "session-env",
            "agents",
            "commands"
        ])
    );
    assert_eq!(
        shared["commands"],
        serde_json::json!(["shared status", "shared enable"])
    );
    assert!(caps["result"].get("root_alias").is_none());
}

#[test]
fn status_and_enable_through_the_cli_and_migrate_is_gone() {
    let f = fixture(&["a", "b"]);
    let status = f.json(&["shared", "status"]);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["accounts"][0]["linked"], false);
    let enabled = f.json(&["shared", "enable"]);
    assert_eq!(enabled["enabled"], true);
    assert!(f.root.join("shared/claude").is_dir());
    assert!(
        !f.run(&["shared", "migrate", "--account", "claude:a"])
            .status
            .success()
    );
    assert!(!f.home("a").join("projects").exists());
}
