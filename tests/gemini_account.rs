//! An enrolled `gemini:<name>` account is deletable and its conversation history is
//! preserved (kept from SHRINK S1b; Gemini CLI support is restored in 1.70.0).
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::{Value, json};
use sessionsation::{
    daemon,
    model::{Account, State},
    paths::Paths,
    provider::ProviderId,
    store::Store,
};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn api(paths: &Paths, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_sessionsation"))
        .arg("--data-dir")
        .arg(&paths.root)
        .arg("api")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enrolled_gemini_account_is_deletable_and_its_history_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("root"))).unwrap();
    {
        let mut store = Store::open(&paths).unwrap();
        let mut state = State::default();
        state.insert_account(Account::with_provider(
            ProviderId::Gemini,
            "gone".into(),
            String::new(),
        ));
        store.save(&state, 1, "test", "fixture").unwrap();
    }
    let home = paths.root.join("accounts/gemini/gone");
    let gemini = home.join(".gemini");
    std::fs::create_dir_all(gemini.join("history")).unwrap();
    std::fs::write(gemini.join("history/marker"), "history").unwrap();
    std::fs::write(gemini.join("oauth_creds.json"), "synthetic").unwrap();
    std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let _daemon = Process(
        Command::new(env!("CARGO_BIN_EXE_sessionsation"))
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["daemon", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon::call(&paths, "status", json!({})).await.is_err() {
        assert!(Instant::now() < deadline, "supervisor did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The provider entry says deletion and removal are supported (and, since 1.70.0, new again).
    let caps = api(&paths, &["capabilities"]);
    let g = caps["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "gemini")
        .unwrap();
    assert_eq!(g["account_deletion"], true);
    assert_eq!(g["account_removal"], true);
    assert_eq!(g["operations"]["account_deletion"]["supported"], true);
    assert_eq!(g["new"], true);
    assert_eq!(g["headless_delivery"]["version"], 1);

    let preview = api(&paths, &["account", "delete", "gemini:gone"]);
    assert_eq!(preview["home_disposition"], "deleted");
    let request = uuid::Uuid::new_v4().to_string();
    api(
        &paths,
        &[
            "account",
            "delete",
            "gemini:gone",
            "--confirm",
            "gemini:gone",
            "--request-id",
            &request,
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let done = loop {
        let r = api(&paths, &["account", "deletions", "--request-id", &request]);
        if matches!(r["phase"].as_str(), Some("complete" | "failed")) {
            break r;
        }
        assert!(Instant::now() < deadline, "deletion did not finish: {r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["history_preserved"], true);
    assert!(!home.exists(), "managed home and credentials removed");
    let vault = paths
        .root
        .join(sessionsation::layout::PRESERVED_CONVERSATIONS)
        .join(&request)
        .join("home/.gemini");
    assert_eq!(
        std::fs::read_to_string(vault.join("history/marker")).unwrap(),
        "history"
    );
    assert!(!Path::new(&vault.join("oauth_creds.json")).exists());
}
