//! `sessionsation shared ...` through the real binary on disposable roots (contract 1.39.0).
use serde_json::Value;
use sessionsation::{
    model::{Account, State},
    paths::Paths,
    provider::ProviderId,
    store::Store,
};
use std::{
    fs,
    path::{Path, PathBuf},
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
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn exchange_supported(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let (x, y) = (dir.join("px"), dir.join("py"));
    fs::create_dir(&x).unwrap();
    std::os::unix::fs::symlink("t", &y).unwrap();
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            c(&x).as_ptr(),
            libc::AT_FDCWD,
            c(&y).as_ptr(),
            2u32,
        )
    };
    let _ = fs::remove_dir(&x).or_else(|_| fs::remove_file(&x));
    let _ = fs::remove_dir(&y).or_else(|_| fs::remove_file(&y));
    r == 0
}

#[test]
fn capabilities_advertise_shared_claude_state() {
    let f = fixture(&[]);
    let caps = f.json(&["api", "capabilities"]);
    assert_eq!(caps["result"]["contract_version"], "1.44.0");
    let shared = &caps["result"]["shared_claude_state"];
    assert_eq!(shared["version"], 1);
    assert_eq!(
        shared["names"],
        serde_json::json!(["projects", "file-history", "todos", "session-env"])
    );
}

#[test]
fn status_enable_and_migrate_through_the_cli() {
    let f = fixture(&["a", "b"]);
    if !exchange_supported(&f.root) {
        eprintln!("SKIPPED: renameat2(RENAME_EXCHANGE) is not supported on this test file system");
        return;
    }
    let status = f.json(&["shared", "status"]);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["accounts"][0]["linked"], false);

    write(&f.home("a").join("projects/-w/one.jsonl"), "one");
    write(&f.home("b").join("projects/-w/two.jsonl"), "two");
    write(&f.home("b").join("todos/t-agent-1.json"), "[]");
    let dry = f.json(&["shared", "migrate", "--account", "claude:a", "--dry-run"]);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["names"]["projects"]["action"], "adopt_by_exchange");
    assert!(!f.root.join("shared").exists(), "dry run wrote nothing");

    for name in ["a", "b"] {
        let out = f.json(&[
            "shared",
            "migrate",
            "--account",
            &format!("claude:{name}"),
            "--allow-full-copy",
        ]);
        assert_eq!(out["complete"], true, "{out}");
    }
    let status = f.json(&["shared", "status"]);
    assert_eq!(status["enabled"], true);
    for account in status["accounts"].as_array().unwrap() {
        assert_eq!(account["linked"], true, "{account}");
        assert_eq!(account["names"]["projects"], "linked");
    }
    for name in ["a", "b"] {
        assert_eq!(
            fs::read_link(f.home(name).join("projects")).unwrap(),
            f.root.join("shared/claude/projects")
        );
        assert!(f.home(name).join("projects/-w/one.jsonl").exists());
        assert!(f.home(name).join("projects/-w/two.jsonl").exists());
    }
    let again = f.json(&["shared", "migrate", "--account", "claude:b"]);
    assert_eq!(again["already_migrated"], true);
}

#[test]
fn conflicts_exit_nonzero_with_a_code_and_change_nothing_destructively() {
    let f = fixture(&["a", "b"]);
    if !exchange_supported(&f.root) {
        eprintln!("SKIPPED: renameat2(RENAME_EXCHANGE) is not supported on this test file system");
        return;
    }
    write(&f.home("a").join("projects/-w/memory/MEMORY.md"), "a");
    write(&f.home("b").join("projects/-w/memory/MEMORY.md"), "b");
    assert!(
        f.run(&[
            "shared",
            "migrate",
            "--account",
            "claude:a",
            "--allow-full-copy"
        ])
        .status
        .success()
    );
    let out = f.run(&[
        "shared",
        "migrate",
        "--account",
        "claude:b",
        "--allow-full-copy",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SHARED_STATE_MIGRATION_INCOMPLETE"),
        "{stderr}"
    );
    assert!(stderr.contains("\"code\""));
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed["complete"], false);
    assert_eq!(
        fs::read_to_string(f.home("b").join("projects/-w/memory/MEMORY.md")).unwrap(),
        "b"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("shared/claude/projects/-w/memory/MEMORY.md")).unwrap(),
        "a"
    );
    let wrong = f.run(&["shared", "migrate", "--account", "codex:a"]);
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("ACCOUNT_PROVIDER_REQUIRED"));
}
