//! Per-provider account names, display/folder names and the backend rename
//! (contract 1.30.0). Disposable data root and fake providers only.
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths, provider::ProviderId};
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
    fn generation(&self, id: &str) -> String {
        self.row(id).unwrap()["generation"]
            .as_str()
            .unwrap()
            .to_owned()
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
    let gemini = install("gemini.js", include_str!("support/fake-gemini.sh"));
    let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
    let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
        gemini_binary: gemini.to_str().unwrap().into(),
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
    f.api(&["account", "add", "gemini:work"]);
    f.api(&["account", "add", "gemini:solo"]);
    assert!(f.home("claude", "work").is_dir() && f.home("gemini", "work").is_dir());
    let ids: Vec<String> = f
        .api(&["account", "list"])
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect();
    for want in ["claude:work", "gemini:work", "gemini:solo"] {
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
    assert!(f.row("gemini:work").unwrap()["display_name"].is_null());
    // Human commands: an ambiguous bare name asks for PROVIDER:NAME, an unambiguous one still works.
    let out = f.run(&["account", "rename", "work", "Other"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("ACCOUNT_AMBIGUOUS"),
        "{err}"
    );
    assert!(
        err.contains("claude:work") && err.contains("gemini:work"),
        "{err}"
    );
    let out = f.run(&["account", "rename", "solo", "Alone"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.row("gemini:solo").unwrap()["display_name"], "Alone");
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
    assert_eq!(f.row("gemini:work").unwrap()["enabled"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_expose_display_folder_mismatch_and_running_sessions() {
    let f = fixture().await;
    f.api(&["account", "add", "claude:main"]);
    let row = f.row("claude:main").unwrap();
    assert_eq!(row["folder_name"], "main");
    assert_eq!(row["name_mismatch"], false);
    assert_eq!(row["running_sessions"], 0);
    assert_eq!(row["backend_rename"]["available"], true);
    assert_eq!(row["backend_rename"]["update_backend"], false);
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
    assert_eq!(row["backend_rename"]["update_backend"], true);
    assert_eq!(row["backend_rename"]["update_backend_target"], "Studio");
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

/// A catalog entry attached to this live test process is a running session.
fn run_session(f: &Fixture, provider: ProviderId, account: &str) {
    let me = std::process::id();
    let stat = std::fs::read_to_string(format!("/proc/{me}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    sessionsation::catalog::record_provider_launch(
        &f.paths,
        provider,
        account,
        &uuid::Uuid::new_v4().to_string(),
        "running",
        None,
        Some(sessionsation::catalog::Attachment {
            host: "tmux".into(),
            target: "%1".into(),
            pid: me,
            process_start: start,
        }),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_rename_is_blocked_by_a_running_session_but_rename_only_is_not() {
    let f = fixture().await;
    f.api(&["account", "add", "gemini:busy"]);
    run_session(&f, ProviderId::Gemini, "busy");
    let generation = f.generation("gemini:busy");
    let e = f.api_error(&[
        "account",
        "rename-backend",
        "gemini:busy",
        "quiet",
        "--expected-generation",
        &generation,
    ]);
    assert!(e.contains("ACCOUNT_SESSIONS_RUNNING"), "{e}");
    assert!(f.home("gemini", "busy").is_dir() && !f.home("gemini", "quiet").exists());
    assert!(f.row("gemini:busy").is_some());
    // Rename only stays available while the session runs.
    let r = f.api(&[
        "account",
        "rename",
        "gemini:busy",
        "--display-name",
        "Shown",
    ]);
    assert_eq!(r["display_name"], "Shown");
    // Nothing was journaled for the refused attempt.
    assert!(
        f.api(&["operations"])
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["kind"] != "account_rename")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_rename_moves_folder_and_id_and_update_backend_matches_the_display_name() {
    let f = fixture().await;
    f.api(&["account", "add", "claude:work"]);
    f.api(&["account", "add", "gemini:work"]);
    f.api(&["account", "add", "claude:taken"]);
    std::fs::write(f.home("claude", "work").join("history-marker"), b"kept").unwrap();
    let generation = f.generation("claude:work");
    // Collision with the same provider; the stale generation; unchanged name.
    let e = f.api_error(&[
        "account",
        "rename-backend",
        "claude:work",
        "taken",
        "--expected-generation",
        &generation,
    ]);
    assert!(e.contains("ACCOUNT_NAME_EXISTS"), "{e}");
    let e = f.api_error(&[
        "account",
        "rename-backend",
        "claude:work",
        "fresh",
        "--expected-generation",
        "bad",
    ]);
    assert!(e.contains("STALE_TARGET"), "{e}");
    let e = f.api_error(&[
        "account",
        "rename-backend",
        "claude:work",
        "work",
        "--expected-generation",
        &generation,
    ]);
    assert!(e.contains("ACCOUNT_NAME_UNCHANGED"), "{e}");
    // Rename & update backend.
    let done = f.api(&[
        "account",
        "rename-backend",
        "claude:work",
        "fresh",
        "--expected-generation",
        &generation,
    ]);
    assert_eq!(done["account_id"], "claude:fresh");
    assert_eq!(done["previous_account_id"], "claude:work");
    assert_eq!(done["folder_name"], "fresh");
    assert_eq!(done["name_mismatch"], false);
    assert_eq!(done["operation"]["phase"], "complete");
    assert!(f.row("claude:work").is_none());
    assert_eq!(
        f.row("claude:fresh").unwrap()["home"],
        json!(f.home("claude", "fresh"))
    );
    assert_eq!(
        std::fs::read(f.home("claude", "fresh").join("history-marker")).unwrap(),
        b"kept"
    );
    assert!(!f.home("claude", "work").exists());
    // The Gemini account with the old name is untouched.
    assert!(f.row("gemini:work").is_some() && f.home("gemini", "work").is_dir());
    // Allowed now: the freed name is free for claude again, and gemini keeps its own.
    f.api(&["account", "add", "claude:work"]);
    // Operation journal listed, terminal.
    let ops = f.api(&["operations"]);
    let op = ops
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["kind"] == "account_rename")
        .unwrap()
        .clone();
    assert_eq!(op["phase"], "complete");
    assert_eq!(op["replays"], false);
    // Update backend: display name differs, no new name given.
    f.api(&[
        "account",
        "rename",
        "claude:fresh",
        "--display-name",
        "Final",
    ]);
    let row = f.row("claude:fresh").unwrap();
    assert_eq!(row["backend_rename"]["update_backend"], true);
    let g = row["generation"].as_str().unwrap().to_owned();
    let done = f.api(&[
        "account",
        "rename-backend",
        "claude:fresh",
        "--expected-generation",
        &g,
    ]);
    assert_eq!(done["account_id"], "claude:Final");
    assert!(done["display_name"].is_null());
    assert!(f.home("claude", "Final").is_dir());
    let row = f.row("claude:Final").unwrap();
    assert_eq!(row["name_mismatch"], false);
    assert_eq!(row["folder_name"], "Final");
    // Nothing to match: a clean account without a display name needs a new name.
    let g = row["generation"].as_str().unwrap().to_owned();
    let e = f.api_error(&[
        "account",
        "rename-backend",
        "claude:Final",
        "--expected-generation",
        &g,
    ]);
    assert!(e.contains("INVALID_ARGUMENT"), "{e}");
    // Recovering a finished operation changes nothing.
    let again = f.api(&["recover", op["id"].as_str().unwrap()]);
    assert_eq!(again["operation"]["phase"], "complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_home_at_a_deferred_legacy_path_moves_to_the_layout_2_path() {
    let f = fixture().await;
    f.api(&["account", "add", "gemini:old"]);
    // Model a home the layout migration deferred: it sits at the legacy path.
    std::fs::create_dir_all(f.paths.root.join("gemini-accounts")).unwrap();
    std::fs::rename(
        f.home("gemini", "old"),
        f.paths.root.join("gemini-accounts/old"),
    )
    .unwrap();
    let thread = "55555555-5555-4555-8555-555555555555";
    let conversations = f.paths.root.join(sessionsation::layout::CONVERSATIONS);
    std::fs::create_dir_all(&conversations).unwrap();
    let record = conversations.join(format!("{thread}.json"));
    let legacy_home = f.paths.root.join("gemini-accounts/old");
    std::fs::write(
        &record,
        serde_json::to_vec(&json!({"owner":legacy_home})).unwrap(),
    )
    .unwrap();
    let row = f.row("gemini:old").unwrap();
    assert_eq!(row["home_deferred"], true);
    let g = row["generation"].as_str().unwrap().to_owned();
    let done = f.api(&[
        "account",
        "rename-backend",
        "gemini:old",
        "new",
        "--expected-generation",
        &g,
    ]);
    assert_eq!(done["account_id"], "gemini:new");
    assert!(f.home("gemini", "new").is_dir());
    assert!(!f.paths.root.join("gemini-accounts/old").exists());
    assert_eq!(f.row("gemini:new").unwrap()["home_deferred"], false);
    // The conversation owner followed the renamed home.
    let owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(owner["owner"], json!(f.home("gemini", "new")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_and_claude_share_a_name_and_a_codex_backend_rename_moves_its_home() {
    let f = fixture().await;
    f.api(&["account", "add", "codex:work"]);
    f.api(&["account", "add", "claude:work"]);
    assert!(f.row("codex:work").is_some() && f.row("claude:work").is_some());
    // The Codex registry key stays the bare name; the other provider is qualified.
    let state = sessionsation::store::load_state_snapshot(&f.paths).unwrap();
    assert!(state.accounts.contains_key("work") && state.accounts.contains_key("claude:work"));
    // Wait until the supervisor's own poll of the Codex account is not in flight.
    let mut last = String::new();
    for _ in 0..50 {
        let g = f.generation("codex:work");
        let out = f.run(&[
            "api",
            "account",
            "rename-backend",
            "codex:work",
            "main",
            "--expected-generation",
            &g,
        ]);
        if out.status.success() {
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(v["result"]["account_id"], "codex:main");
            last.clear();
            break;
        }
        last = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(last.contains("ACCOUNT_POLL_IN_PROGRESS"), "{last}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(last.is_empty(), "{last}");
    assert!(f.home("codex", "main").is_dir() && !f.home("codex", "work").exists());
    assert!(f.row("codex:work").is_none() && f.row("claude:work").is_some());
    let state = sessionsation::store::load_state_snapshot(&f.paths).unwrap();
    assert!(state.accounts.contains_key("main") && !state.accounts.contains_key("work"));
    assert!(state.accounts.contains_key("claude:work"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_names_collide_within_one_provider_only() {
    let f = fixture().await;
    f.api(&["account", "add", "claude:work"]);
    f.api(&["account", "add", "claude:other"]);
    f.api(&["account", "add", "gemini:work"]);
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
            "gemini:work",
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
    // Backend rename into another account's display name of the same provider collides too.
    let g = f.generation("claude:other");
    let e = f.api_error(&[
        "account",
        "rename-backend",
        "claude:other",
        "Office",
        "--expected-generation",
        &g,
    ]);
    assert!(e.contains("ACCOUNT_NAME_EXISTS"), "{e}");
}
