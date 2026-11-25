//! Data-root layout: a legacy (version 1) root fails closed with a version message
//! and still reports `layout status`, a new root is stamped version 2, and an
//! unfinished layout change is refused.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use sessionsation::layout;
use std::{fs, process::Command};

#[test]
fn cli_refuses_a_legacy_root_but_reports_version_and_layout() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("registry.db"), "").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_sessionsation"))
            .arg("--data-dir")
            .arg(&root)
            .args(args)
            .env_remove("SESSIONSATION_HOME")
            .output()
            .unwrap()
    };
    let state = run(&["api", "state", "--cached"]);
    assert!(!state.status.success());
    let text = String::from_utf8_lossy(&state.stdout) + String::from_utf8_lossy(&state.stderr);
    assert!(text.contains("LAYOUT_MIGRATION_REQUIRED"), "{text}");
    assert!(text.contains("layout version 1 is not supported"), "{text}");
    assert!(
        run(&["api", "--api-version", "1", "version"])
            .status
            .success()
    );
    let status = run(&["layout", "status"]);
    assert!(status.status.success());
    let v: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(v["layout_version"], 1);
    assert_eq!(v["migration_required"], true);
    // The removed verbs are refused by the parser.
    assert!(!run(&["layout", "migrate"]).status.success());
    assert!(!run(&["layout", "rollback"]).status.success());
}

#[test]
fn a_new_root_is_created_in_layout_version_two() {
    let t = tempfile::tempdir().unwrap();
    let paths = sessionsation::paths::Paths::new(Some(t.path().join("new"))).unwrap();
    let r = layout::read_record(&paths.root).unwrap().unwrap();
    assert_eq!(r.version, layout::VERSION);
    assert_eq!(
        paths.account("work").unwrap(),
        paths.root.join("accounts/codex/work")
    );
    let status = layout::status(&paths.root).unwrap();
    assert_eq!(status["layout_version"], 2);
    assert_eq!(status["migration_required"], false);
}

#[test]
fn an_unsupported_or_unfinished_layout_is_refused_with_its_version() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    fs::create_dir_all(&root).unwrap();
    for (record, code, text) in [
        (
            r#"{"version":1,"state":"complete"}"#,
            "LAYOUT_MIGRATION_REQUIRED",
            "layout version 1 is not supported; this release needs 3",
        ),
        (
            r#"{"version":2,"state":"migrating"}"#,
            "LAYOUT_MIGRATION_IN_PROGRESS",
            "did not finish",
        ),
        (
            r#"{"version":4,"state":"complete"}"#,
            "LAYOUT_UNSUPPORTED",
            "newer than this build",
        ),
    ] {
        fs::write(root.join(layout::LAYOUT_FILE), record).unwrap();
        let e = layout::admit(&root).unwrap_err().to_string();
        assert!(e.starts_with(code) && e.contains(text), "{e}");
    }
}
