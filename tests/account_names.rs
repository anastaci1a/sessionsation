//! Per-provider account names and display/folder names
//! (contract 1.30.0). Disposable data root and fake providers only.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGKILL);
            }
        }
        let _ = self.0.wait();
    }
}
struct Fixture {
    _tmp: tempfile::TempDir,
    paths: Paths,
    _daemon: Process,
}
impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(binary())
            .arg("--data-dir")
            .arg(&self.paths.root)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("HERDR_ENV")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn api(&self, args: &[&str]) -> Value {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    }
    fn api_error(&self, args: &[&str]) -> String {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn row(&self, id: &str) -> Option<Value> {
        self.api(&["account", "list"])
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .cloned()
    }
    fn home(&self, platform: &str, name: &str) -> PathBuf {
        self.paths.root.join("accounts").join(platform).join(name)
    }
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let install = |name: &str, body: &str| {
        let path = tmp.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let gemini = install("agy", include_str!("support/fake-agy.sh"));
    let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
    let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
        antigravity_binary: gemini.to_str().unwrap().into(),
        ..Default::default()
    };
    sessionsation::store::save_config(&paths, &config).unwrap();
    let daemon = Process(
        Command::new(binary())
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
        assert!(
            Instant::now() < deadline,
            "fixture supervisor did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Fixture {
        _tmp: tmp,
        paths,
        _daemon: daemon,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_name_can_exist_once_per_provider_and_bare_names_must_be_unambiguous() {
    let f = fixture().await;
    f.api(&["account", "add", "claude:work"]);
    f.api(&["account", "add", "antigravity:work"]);
    f.api(&["account", "add", "antigravity:solo"]);
    assert!(f.home("claude", "work").is_dir() && f.home("antigravity", "work").is_dir());
    let ids: Vec<String> = f
        .api(&["account", "list"])
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect();
    for want in ["claude:work", "antigravity:work", "antigravity:solo"] {
        assert!(ids.iter().any(|i| i == want), "{ids:?}");
    }
    // The same provider cannot hold the name twice.
    let e = f.api_error(&["account", "add", "claude:work"]);
    assert!(e.contains("ACCOUNT_NAME_EXISTS"), "{e}");
    // Qualified ids address the right account; the other one is untouched.
    f.api(&[
        "account",
        "rename",
        "claude:work",
        "--display-name",
        "Office",
    ]);
    assert_eq!(f.row("claude:work").unwrap()["display_name"], "Office");
    assert!(f.row("antigravity:work").unwrap()["display_name"].is_null());
    // Human commands: an ambiguous bare name asks for PROVIDER:NAME, an unambiguous one still works.
    let out = f.run(&["account", "rename", "work", "Other"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("ACCOUNT_AMBIGUOUS"),
        "{err}"
    );
    assert!(
        err.contains("claude:work") && err.contains("antigravity:work"),
        "{err}"
    );
    let out = f.run(&["account", "rename", "solo", "Alone"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.row("antigravity:solo").unwrap()["display_name"], "Alone");
    // The supervisor itself refuses a bare name that matches two providers.
    let e = daemon::call(&f.paths, "account/disable", json!({"name":"work"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("ACCOUNT_AMBIGUOUS"), "{e}");
    // A provider-qualified supervisor request works.
    daemon::call(
        &f.paths,
        "account/disable",
        json!({"name":"work","provider":"claude"}),
    )
    .await
    .unwrap();
    assert_eq!(f.row("claude:work").unwrap()["enabled"], false);
    assert_eq!(f.row("antigravity:work").unwrap()["enabled"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_expose_display_folder_mismatch_and_running_sessions() {
    let f = fixture().await;
    f.api(&["account", "add", "claude:main"]);
    let row = f.row("claude:main").unwrap();
    assert_eq!(row["folder_name"], "main");
    assert_eq!(row["name_mismatch"], false);
    assert_eq!(row["running_sessions"], 0);
    assert!(
        row.get("backend_rename").is_none(),
        "the backend rename is gone"
    );
    // Rename only: display name changes, folder and id do not.
    let renamed = f.api(&[
        "account",
        "rename",
        "claude:main",
        "--display-name",
        "Studio",
    ]);
    assert_eq!(renamed["account_id"], "claude:main");
    assert_eq!(renamed["display_name"], "Studio");
    assert_eq!(renamed["name_mismatch"], true);
    let row = f.row("claude:main").unwrap();
    assert_eq!(row["display_name"], "Studio");
    assert_eq!(row["folder_name"], "main");
    assert_eq!(row["name_mismatch"], true);
    assert!(f.home("claude", "main").is_dir());
    // A stale generation is refused.
    let e = f.api_error(&[
        "account",
        "rename",
        "claude:main",
        "--display-name",
        "X",
        "--expected-generation",
        "0000",
    ]);
    assert!(e.contains("STALE_TARGET"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_names_collide_within_one_provider_only() {
    let f = fixture().await;
    f.api(&["account", "add", "claude:work"]);
    f.api(&["account", "add", "claude:other"]);
    f.api(&["account", "add", "antigravity:work"]);
    f.api(&[
        "account",
        "rename",
        "claude:work",
        "--display-name",
        "Office",
    ]);
    // The same display name or another account's name under another provider is free.
    assert_eq!(
        f.api(&[
            "account",
            "rename",
            "antigravity:work",
            "--display-name",
            "Office"
        ])["display_name"],
        "Office"
    );
    // Within the provider it collides with a display name and with a name.
    for display in ["Office", "work"] {
        let e = f.api_error(&[
            "account",
            "rename",
            "claude:other",
            "--display-name",
            display,
        ]);
        assert!(e.contains("ACCOUNT_DISPLAY_NAME_EXISTS"), "{e}");
    }
}
