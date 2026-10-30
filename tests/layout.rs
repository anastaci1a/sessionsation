//! Data-root layout migration (version 1 -> 2) over a realistic legacy tree with
//! all four providers: in-use homes are deferred (never moved), idle homes move
//! with a compatibility symlink (Codex control socket and SQLite paths keep
//! working through it), and migrate is idempotent, resumable and reversible.
use sessionsation::layout::{self, migrate};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

const ROLLOUT: &str = "sessions/2026/10/29/rollout-2026-10-29T09-20-33-01a187d1.jsonl";

struct User(Child);
impl Drop for User {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
/// A process that uses `home` the way providers do: through its environment.
fn user(var: &str, home: &Path) -> User {
    User(
        Command::new("sleep")
            .arg("60")
            .env(var, home)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A legacy root shaped like the live one: WAL registry with unflushed pages,
/// receipts, journals with absolute paths, logs, cache and four providers' homes.
/// The returned connection keeps the registry WAL un-checkpointed.
fn legacy_root(root: &Path) -> rusqlite::Connection {
    fs::create_dir_all(root).unwrap();
    let r = root.display();
    let db = rusqlite::Connection::open(root.join("registry.db")).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE state(id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL);
         INSERT INTO state VALUES(1, '{\"accounts\":{\"personal\":{}}}');",
    )
    .unwrap();
    fs::create_dir_all(root.join("messaging/checkpoints")).unwrap();
    rusqlite::Connection::open(root.join("messaging/receipts.db"))
        .unwrap()
        .execute_batch("CREATE TABLE receipts(id TEXT); INSERT INTO receipts VALUES('m1');")
        .unwrap();
    write(&root.join("config.json"), "{}");
    write(&root.join("session-access.json"), "{}");
    write(&root.join("session-access.lock"), "");
    write(&root.join("daemon.log"), "daemon\n");
    write(&root.join("handoff.log"), "handoff\n");
    write(&root.join("state-snapshot-active.json"), "{}");
    write(&root.join("compatibility-report.json"), "{}");
    write(&root.join("project-relocation.lock"), "");
    for dir in [
        "account-catalog/locks",
        "session-catalog",
        "conversations",
        "thread-locations",
        "managed-panes",
        "herdr-panes",
        "provider-state/gemini-auth",
        "claude-hosted",
        "launches",
        "resumptions",
        "deletions",
        "account-deletions",
        "conversation-operations",
        "handoff-jobs",
        "space-moves",
        "lifecycle-admission",
        "message-recoveries",
        "move-backups",
        "claude-steer",
        "claude-deliveries",
        "questions/answers",
        "observations",
        "sessions",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    write(
        &root.join("session-catalog/catalog.json"),
        r#"{"sessions":[]}"#,
    );
    write(&root.join("launches/l1.json"), r#"{"phase":"running"}"#);
    write(&root.join("observations/s1.json"), "{}");
    write(
        &root.join("preserved-conversations/d1/account.json"),
        &format!(r#"{{"home":"{r}/preserved-conversations/d1/home"}}"#),
    );
    write(
        &root.join("preserved-conversations/d1/home/history.jsonl"),
        "h\n",
    );
    write(
        &root.join("archived-sessions/a1/manifest.json"),
        &format!(
            r#"{{"home":"{r}/accounts/personal","rollout":"{r}/accounts/personal/{ROLLOUT}"}}"#
        ),
    );
    write(
        &root.join("handoff-jobs/j1.json"),
        &format!(r#"{{"phase":"complete","home":"{r}/archived-sessions/a1/home"}}"#),
    );
    // Codex: two homes with rollouts and a state database recording absolute paths.
    for name in ["personal", "gate1"] {
        let home = root.join("accounts").join(name);
        write(&home.join("auth.json"), "synthetic");
        write(&home.join(ROLLOUT), "{}\n");
        fs::create_dir_all(home.join("app-server-control")).unwrap();
        let c = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE threads(id TEXT, rollout_path TEXT);")
            .unwrap();
        c.execute(
            "INSERT INTO threads VALUES('t', ?1)",
            [home.join(ROLLOUT).to_string_lossy()],
        )
        .unwrap();
    }
    // Claude, Gemini, Antigravity homes.
    for (parent, name, file) in [
        ("claude-accounts", "claude-main", ".credentials.json"),
        ("claude-accounts", "spare", ".credentials.json"),
        ("gemini-accounts", "gemmy", ".gemini/settings.json"),
        (
            "antigravity-accounts",
            "ana",
            ".gemini/antigravity-cli/settings.json",
        ),
    ] {
        write(&root.join(parent).join(name).join(file), "synthetic");
    }
    assert!(
        fs::metadata(root.join("registry.db-wal")).unwrap().len() > 0,
        "the registry WAL holds the state row"
    );
    db
}

/// Every regular file and symlink under `root` with its content (or target),
/// keyed by relative path; lock files and the layout record are ignored.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let t = e.file_type().unwrap();
            if t.is_symlink() {
                out.insert(rel, format!("-> {}", fs::read_link(&p).unwrap().display()));
            } else if t.is_dir() {
                walk(root, &p, out);
            } else if !t.is_file() {
                out.insert(rel, "special".into());
            } else if !rel.ends_with(".lock") && rel != "layout.json" && !rel.ends_with("-shm") {
                let bytes = fs::read(&p).unwrap();
                out.insert(rel, format!("{} bytes", bytes.len()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}
fn registry_state(db: &Path) -> String {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT json FROM state", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn migration_defers_in_use_homes_moves_idle_ones_and_rolls_back() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("root");
    let _db = legacy_root(&root);
    let root = fs::canonicalize(&root).unwrap();
    // A legacy root fails closed for normal commands; `layout` still works.
    let e = sessionsation::paths::Paths::open(Some(root.clone()))
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("LAYOUT_MIGRATION_REQUIRED"), "{e}");
    assert_eq!(migrate::status(&root).unwrap()["migration_required"], true);

    // In use: Codex personal (app-server environment), Claude claude-main
    // (CLAUDE_CONFIG_DIR) and Antigravity ana (HOME).
    let codex_user = user("CODEX_HOME", &root.join("accounts/personal"));
    let _claude_user = user(
        "CLAUDE_CONFIG_DIR",
        &root.join("claude-accounts/claude-main"),
    );
    let _agy_user = user("HOME", &root.join("antigravity-accounts/ana"));
    // An idle Codex home whose control socket is bound by a live listener (this
    // test process, which the scan excludes) and whose SQLite records paths.
    let socket = root.join("accounts/gate1/app-server-control/app-server-control.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let dry = migrate::migrate(
        &root,
        &migrate::Options {
            backup: None,
            dry_run: true,
        },
    )
    .unwrap();
    assert!(root.join("registry.db").is_file() && !root.join("layout.json").exists());
    assert_eq!(
        dry["plan"]["deferred"].as_array().unwrap().len(),
        3,
        "{dry}"
    );
    let e = migrate::migrate(&root, &migrate::Options::default())
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("BACKUP_REQUIRED"), "{e}");
    // The supervisor's lock blocks a migration.
    {
        use fs2::FileExt;
        let held = fs::File::create(root.join("daemon.lock")).unwrap();
        held.lock_exclusive().unwrap();
        let e = migrate::migrate(&root, &migrate::Options::default())
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("DAEMON_RUNNING"), "{e}");
    }
    let before = snapshot(&root);
    let present: Vec<_> = layout::MOVES
        .iter()
        .filter(|(legacy, _)| root.join(legacy).exists())
        .collect();
    assert!(present.len() >= 30, "realistic fixture");
    let backup = t.path().join("backup");
    let out = migrate::migrate(
        &root,
        &migrate::Options {
            backup: Some(backup.clone()),
            dry_run: false,
        },
    )
    .unwrap();

    // Backup: metadata and consistent database copies, never account homes.
    assert!(backup.join("layout-backup.json").is_file());
    assert_eq!(
        registry_state(&backup.join("registry.db")),
        r#"{"accounts":{"personal":{}}}"#
    );
    assert!(backup.join("messaging/receipts.db").is_file());
    assert!(backup.join("launches/l1.json").is_file());
    assert!(!backup.join("accounts").exists() && !backup.join("claude-accounts").exists());

    // Stores moved; the WAL content survived; legacy registry path is a tripwire.
    assert_eq!(
        registry_state(&root.join(layout::REGISTRY_DB)),
        r#"{"accounts":{"personal":{}}}"#
    );
    assert_eq!(
        fs::read_to_string(root.join("registry.db")).unwrap(),
        migrate::TRIPWIRE
    );
    for (legacy, new) in present {
        if *legacy != "registry.db" && !layout::COMPAT_LINKS.contains(legacy) {
            assert!(!root.join(legacy).exists(), "{legacy} still present");
        }
        assert!(
            fs::symlink_metadata(root.join(new)).is_ok(),
            "{new} missing"
        );
    }
    for d in [
        "registry",
        "sessions",
        "operations",
        "messaging",
        "cache",
        "logs",
    ] {
        assert!(root.join(d).is_dir(), "{d}");
    }
    assert_eq!(
        fs::read_to_string(root.join(layout::DAEMON_LOG)).unwrap(),
        "daemon\n"
    );
    // Recorded absolute paths keep resolving through compatibility links.
    for legacy in layout::COMPAT_LINKS {
        assert!(
            fs::symlink_metadata(root.join(legacy))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    let job: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("operations/handoff-jobs/j1.json")).unwrap())
            .unwrap();
    assert_eq!(
        job["home"],
        format!("{}/archived-sessions/a1/home", root.display())
    );
    let preserved: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("sessions/preserved/d1/account.json")).unwrap())
            .unwrap();
    assert!(
        Path::new(preserved["home"].as_str().unwrap())
            .join("history.jsonl")
            .is_file()
    );

    // In-use homes stayed exactly where they were, with the reason recorded.
    let record = layout::read_record(&root).unwrap().unwrap();
    assert_eq!(record.version, layout::VERSION);
    let deferred: BTreeMap<_, _> = record
        .deferred
        .iter()
        .map(|d| (format!("{}:{}", d.provider, d.name), d.reason.clone()))
        .collect();
    assert_eq!(
        deferred.keys().cloned().collect::<Vec<_>>(),
        ["antigravity:ana", "claude:claude-main", "codex:personal"]
    );
    assert!(deferred["codex:personal"].contains(&format!("pid {}", codex_user.0.id())));
    assert!(deferred["codex:personal"].contains("env CODEX_HOME"));
    for (provider, name, legacy) in [
        (
            sessionsation::provider::ProviderId::Codex,
            "personal",
            "accounts/personal",
        ),
        (
            sessionsation::provider::ProviderId::Claude,
            "claude-main",
            "claude-accounts/claude-main",
        ),
        (
            sessionsation::provider::ProviderId::Antigravity,
            "ana",
            "antigravity-accounts/ana",
        ),
    ] {
        assert!(fs::symlink_metadata(root.join(legacy)).unwrap().is_dir());
        assert_eq!(
            layout::account_home(&root, provider, name),
            root.join(legacy)
        );
    }
    assert!(
        out["deferred"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["reason"].as_str().unwrap().starts_with("in use: pid "))
    );

    // Idle homes moved to accounts/<platform>/<name> with a compatibility link.
    for (provider, name, legacy, link) in [
        (
            sessionsation::provider::ProviderId::Codex,
            "gate1",
            "accounts/gate1",
            "../accounts/codex/gate1",
        ),
        (
            sessionsation::provider::ProviderId::Claude,
            "spare",
            "claude-accounts/spare",
            "../accounts/claude/spare",
        ),
        (
            sessionsation::provider::ProviderId::Gemini,
            "gemmy",
            "gemini-accounts/gemmy",
            "../accounts/gemini/gemmy",
        ),
    ] {
        let target = layout::target_home(&root, provider, name);
        assert!(fs::symlink_metadata(&target).unwrap().is_dir(), "{name}");
        assert_eq!(
            fs::read_link(root.join(legacy)).unwrap(),
            PathBuf::from(link)
        );
        assert_eq!(layout::account_home(&root, provider, name), target);
        assert!(layout::same_home(&root.join(legacy), &target));
    }
    // Codex: the control socket and the state database recorded paths work
    // through the old path, and the bound listener still accepts.
    let old_socket = root.join("accounts/gate1/app-server-control/app-server-control.sock");
    let _client = std::os::unix::net::UnixStream::connect(&old_socket).unwrap();
    listener.accept().unwrap();
    let recorded: String = rusqlite::Connection::open(root.join("accounts/gate1/state_5.sqlite"))
        .unwrap()
        .query_row("SELECT rollout_path FROM threads", [], |r| r.get(0))
        .unwrap();
    assert!(recorded.contains("/accounts/gate1/") && Path::new(&recorded).is_file());
    let homes: Vec<_> = layout::codex_homes(&root)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(homes, ["gate1", "personal"]);
    // The build now admits the root.
    sessionsation::paths::Paths::open(Some(root.clone())).unwrap();

    // Idempotent: a second run changes nothing and needs no backup.
    let again = migrate::migrate(&root, &migrate::Options::default()).unwrap();
    assert_eq!(again["stores_moved"], 0);
    assert_eq!(again["homes_moved"].as_array().unwrap().len(), 0);
    assert_eq!(again["deferred"].as_array().unwrap().len(), 3);

    // Resumable: once idle, a later run moves a deferred home.
    drop(codex_user);
    let e = migrate::migrate(&root, &migrate::Options::default())
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("BACKUP_REQUIRED"), "{e}");
    let later = migrate::migrate(
        &root,
        &migrate::Options {
            backup: Some(t.path().join("backup-2")),
            dry_run: false,
        },
    )
    .unwrap();
    assert_eq!(later["homes_moved"][0]["name"], "personal");
    assert!(root.join("accounts/codex/personal/auth.json").is_file());
    assert_eq!(
        fs::read_link(root.join("accounts/personal")).unwrap(),
        PathBuf::from("../accounts/codex/personal")
    );
    let status = migrate::status(&root).unwrap();
    assert_eq!(status["deferred"].as_array().unwrap().len(), 2);
    assert_eq!(status["deferred"][0]["in_use_now"], true);

    // A rollback refuses a home in use at its new location.
    let busy = user("GEMINI_CLI_HOME", &root.join("accounts/gemini/gemmy"));
    let e = migrate::rollback(&root, false).unwrap_err().to_string();
    assert!(
        e.starts_with("LAYOUT_CONFLICT") && e.contains("in use"),
        "{e}"
    );
    assert!(root.join("layout.json").is_file());
    drop(busy);
    drop(listener);
    fs::remove_file(root.join("accounts/codex/gate1/app-server-control/app-server-control.sock"))
        .unwrap();

    // Rollback restores the legacy tree exactly (files, contents, homes).
    let back = migrate::rollback(&root, false).unwrap();
    assert_eq!(back["rolled_back"], true);
    let mut expected = before.clone();
    expected.remove("accounts/gate1/app-server-control/app-server-control.sock");
    let mut after = snapshot(&root);
    // The WAL was folded into the database; compare the database by content.
    for snap in [&mut expected, &mut after] {
        snap.remove("registry.db");
        snap.remove("registry.db-wal");
    }
    assert_eq!(after, expected);
    assert_eq!(
        registry_state(&root.join("registry.db")),
        r#"{"accounts":{"personal":{}}}"#
    );
    assert!(!root.join("layout.json").exists());
    assert!(
        layout::admit(&root)
            .unwrap_err()
            .to_string()
            .starts_with("LAYOUT_MIGRATION_REQUIRED")
    );
    // And a rollback of a legacy root is a no-op.
    assert_eq!(
        migrate::rollback(&root, false).unwrap()["rolled_back"],
        false
    );
}

#[test]
fn conflicts_stop_before_any_change_and_an_interrupted_run_resumes() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("root");
    let _db = legacy_root(&root);
    // Both a legacy store and its new location: nothing moves.
    write(&root.join("operations/launches/other.json"), "{}");
    let before = snapshot(&root);
    let e = migrate::migrate(
        &root,
        &migrate::Options {
            backup: Some(t.path().join("b0")),
            dry_run: false,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.starts_with("LAYOUT_CONFLICT") && e.contains("launches"),
        "{e}"
    );
    assert_eq!(snapshot(&root), before);
    assert!(!t.path().join("b0").exists());
    fs::remove_dir_all(root.join("operations")).unwrap();

    // Interrupted after a few moves (state `migrating`): normal commands fail
    // closed and the next run completes from where it stopped.
    fs::create_dir_all(root.join("operations")).unwrap();
    fs::rename(root.join("launches"), root.join(layout::LAUNCHES)).unwrap();
    fs::create_dir_all(root.join("accounts/gemini")).unwrap();
    fs::rename(
        root.join("gemini-accounts/gemmy"),
        root.join("accounts/gemini/gemmy"),
    )
    .unwrap();
    layout::write_record(
        &root,
        &layout::Record {
            version: 1,
            state: layout::State::Migrating,
            ..Default::default()
        },
    )
    .unwrap();
    let e = layout::admit(&root).unwrap_err().to_string();
    assert!(e.starts_with("LAYOUT_MIGRATION_IN_PROGRESS"), "{e}");
    migrate::migrate(
        &root,
        &migrate::Options {
            backup: Some(t.path().join("b1")),
            dry_run: false,
        },
    )
    .unwrap();
    layout::admit(&root).unwrap();
    assert!(root.join("operations/launches/l1.json").is_file());
    assert!(
        root.join("accounts/gemini/gemmy/.gemini/settings.json")
            .is_file()
    );
    assert!(
        root.join("accounts/claude/claude-main/.credentials.json")
            .is_file()
    );
    assert!(root.join(layout::REGISTRY_DB).is_file());
}

#[test]
fn cli_refuses_a_legacy_root_but_reports_version_and_layout() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("root");
    drop(legacy_root(&root));
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
    assert!(
        String::from_utf8_lossy(&state.stdout).contains("LAYOUT_MIGRATION_REQUIRED")
            || String::from_utf8_lossy(&state.stderr).contains("LAYOUT_MIGRATION_REQUIRED")
    );
    assert!(
        run(&["api", "--api-version", "1", "version"])
            .status
            .success()
    );
    let status = run(&["layout", "status"]);
    assert!(status.status.success());
    let v: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(v["layout_version"], 1);
    let backup = t.path().join("b");
    let m = run(&["layout", "migrate", "--backup", backup.to_str().unwrap()]);
    assert!(m.status.success(), "{}", String::from_utf8_lossy(&m.stderr));
    let v: serde_json::Value = serde_json::from_slice(&run(&["layout", "status"]).stdout).unwrap();
    assert_eq!(v["layout_version"], 2);
    let r = run(&["retention", "run", "--dry-run"]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let v: serde_json::Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(v["dry_run"], true);
    // An older build would find a non-database at the legacy registry path.
    let e = rusqlite::Connection::open(root.join("registry.db"))
        .unwrap()
        .execute_batch("CREATE TABLE IF NOT EXISTS x(y)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("not a database"), "{e}");
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
    // Nothing to migrate on a version 2 root: no backup, links or tripwire.
    fs::create_dir_all(paths.root.join(layout::ARCHIVED_SESSIONS)).unwrap();
    fs::create_dir_all(paths.root.join(layout::REGISTRY)).unwrap();
    rusqlite::Connection::open(paths.root.join(layout::REGISTRY_DB))
        .unwrap()
        .execute_batch("CREATE TABLE state(id INTEGER PRIMARY KEY, json TEXT NOT NULL)")
        .unwrap();
    let r = migrate::migrate(&paths.root, &migrate::Options::default()).unwrap();
    assert_eq!(r["stores_moved"], 0);
    assert!(fs::symlink_metadata(paths.root.join("archived-sessions")).is_err());
    assert!(fs::symlink_metadata(paths.root.join("registry.db")).is_err());
}
