//! `root migrate | status | rollback | finalize` (contract 1.54.0) against a disposable
//! fixture root with the metadata-only fake Claude and a fake HERDR.
//!
//! Bind mounts need root, so the fixture fakes the two spellings with symlinks
//! (`<tmp>/user/{appdata,live}` -> `<tmp>/disk/{appdata,live}`) and the debug-only
//! `SESSIONSATION_TEST_ALIAS_PROOF=symlink` proof (never honoured by release builds).
//! A symlink makes `canonicalize` agree across spellings, so the comparison sites
//! that a real bind would expose (F2) are covered by the unit tests in
//! `src/root_migrate/tests.rs` (two real, distinct directories) and by the ignored
//! namespace test at the end of this file.
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
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
    tmp: tempfile::TempDir,
    paths: Paths,
    path: String,
    daemon: Option<Process>,
    old_project: PathBuf,
    new_project: PathBuf,
}
fn key(d: &Path) -> String {
    d.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        std::fs::create_dir_all(t.join("disk/appdata")).unwrap();
        std::fs::create_dir_all(t.join("disk/live/p")).unwrap();
        std::fs::create_dir_all(t.join("user")).unwrap();
        std::os::unix::fs::symlink(t.join("disk/appdata"), t.join("user/appdata")).unwrap();
        std::os::unix::fs::symlink(t.join("disk/live"), t.join("user/live")).unwrap();
        let install = |name: &str, body: &str| {
            let path = t.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
        let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
        let bin = t.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let herdr = bin.join("herdr");
        std::fs::write(&herdr, include_str!("support/fake-herdr.sh")).unwrap();
        std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let paths = Paths::new(Some(t.join("user/appdata/state"))).unwrap();
        let config = sessionsation::config::Config {
            codex_binary: codex.to_str().unwrap().into(),
            claude_binary: claude.to_str().unwrap().into(),
            ..Default::default()
        };
        sessionsation::store::save_config(&paths, &config).unwrap();
        let mut f = Self {
            old_project: t.join("user/live/p"),
            new_project: t.join("disk/live/p"),
            tmp,
            paths,
            path,
            daemon: None,
        };
        f.start_daemon().await;
        assert!(f.run(&["shared", "enable"]).status.success());
        let out = f.run(&["account", "add", "--platform", "claude", "a"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        wait_for("auth status poll", || {
            f.api(&["account", "list"])
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == "claude:a" && a["auth_required"] == false)
        });
        f
    }
    async fn start_daemon(&mut self) {
        let daemon = Process(
            self.command(&["daemon", "serve"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon::call(&self.paths, "status", json!({}))
            .await
            .is_err()
        {
            assert!(
                Instant::now() < deadline,
                "fixture supervisor did not start"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.daemon = Some(daemon);
    }
    fn stop_daemon(&mut self) {
        self.daemon = None; // SIGKILL of the fixture's own supervisor child
        let lock = self.paths.root.join("daemon.lock");
        wait_for("supervisor lock release", || {
            let f = std::fs::OpenOptions::new().write(true).open(&lock);
            f.is_err() || fs2::FileExt::try_lock_exclusive(&f.unwrap()).is_ok()
        });
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(binary());
        c.arg("--data-dir")
            .arg(&self.paths.root)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("HERDR_ENV")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("PATH", &self.path)
            .env("FAKE_HERDR_DIR", self.tmp.path().join("herdr-state"))
            .env("SESSIONSATION_TEST_ALIAS_PROOF", "symlink")
            .stdin(Stdio::null());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v.get("result").cloned().unwrap_or(v)
    }
    fn api(&self, args: &[&str]) -> Value {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        self.json(&all)
    }
    fn home(&self) -> PathBuf {
        self.paths.root.join("accounts/claude/a")
    }
    fn session(&self, id: &str) -> Value {
        self.api(&["state", "--cached"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn live_session(&self, id: &str) -> Value {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }
    /// A stopped Claude session whose transcript records the old-spelling cwd.
    fn stopped_session(&self, cwd: &Path) -> (String, String) {
        let native = uuid::Uuid::new_v4().to_string();
        let dir = self.home().join("projects").join(key(cwd));
        std::fs::create_dir_all(dir.join(&native).join("tool-results")).unwrap();
        std::fs::write(dir.join(&native).join("tool-results/r.txt"), "side").unwrap();
        std::fs::write(
            dir.join(format!("{native}.jsonl")),
            format!(
                "{}\n",
                json!({"type":"user","sessionId":native,"cwd":cwd,"timestamp":"2026-01-01T00:00:00Z"})
            ),
        )
        .unwrap();
        let mut id = String::new();
        wait_for("catalog entry", || {
            let s = self.api(&["state"])["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["conversation_id"] == native.as_str())
                .cloned();
            if let Some(s) = s {
                id = s["id"].as_str().unwrap().to_owned();
                true
            } else {
                false
            }
        });
        (id, native)
    }
    fn catalog_directory(&self, id: &str) -> Option<String> {
        let c: Value = serde_json::from_slice(
            &std::fs::read(
                self.paths
                    .root
                    .join("registry/session-catalog/catalog.json"),
            )
            .unwrap(),
        )
        .unwrap();
        c["sessions"][id]["directory"].as_str().map(str::to_owned)
    }
}

fn file_digest(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(std::fs::read(p).unwrap_or_default()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_r_dry_run_migrate_status_gate_second_run_and_rollback() {
    let mut f = Fixture::new().await;
    let t = f.tmp.path().to_path_buf();
    let old = f.old_project.clone();
    let new = f.new_project.clone();
    // S1: stopped, re-keyable; memory in the old key.
    let (s1, n1) = f.stopped_session(&old);
    let projects = f.home().join("projects");
    std::fs::create_dir_all(projects.join(key(&old)).join("memory")).unwrap();
    std::fs::write(
        projects.join(key(&old)).join("memory/MEMORY.md"),
        "old memory",
    )
    .unwrap();
    // The target key exists already with another session's files and memory.
    let target = projects.join(key(&new));
    std::fs::create_dir_all(target.join("memory")).unwrap();
    std::fs::write(target.join("someone-else.jsonl"), "{}\n").unwrap();
    std::fs::write(target.join("memory/theirs.md"), "theirs").unwrap();
    // S2: a pre-existing `<id>/` of the same name in the target key: collision.
    let other = f.tmp.path().join("user/live/q");
    std::fs::create_dir_all(f.tmp.path().join("disk/live/q")).unwrap();
    let (s2, n2) = f.stopped_session(&other);
    let q_target = projects.join(key(&f.tmp.path().join("disk/live/q")));
    std::fs::create_dir_all(q_target.join(&n2)).unwrap();
    f.stop_daemon();
    // Old-spelling records in every rewritten class.
    let root_old = t.join("user/appdata/state");
    let reg = f.paths.root.join("registry");
    let owner = reg
        .join("conversations")
        .join(format!("{}.json", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(owner.parent().unwrap()).unwrap();
    std::fs::write(
        &owner,
        serde_json::to_vec(&json!({"owner":root_old.join("accounts/codex/c"),"x":1})).unwrap(),
    )
    .unwrap();
    let pane = reg.join("managed-panes").join("pane.json");
    std::fs::create_dir_all(pane.parent().unwrap()).unwrap();
    std::fs::write(
        &pane,
        serde_json::to_vec_pretty(&json!({"home":root_old.join("accounts/codex/c")})).unwrap(),
    )
    .unwrap();
    let location = reg
        .join("thread-locations")
        .join(format!("{}.json", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(location.parent().unwrap()).unwrap();
    std::fs::write(&location, serde_json::to_vec(&json!({"cwd":old})).unwrap()).unwrap();
    let link = f.home().join("projects");
    let store_old = root_old.join("shared/claude/projects");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&store_old, &link).unwrap();
    let digests: Vec<(PathBuf, String)> = [&owner, &pane, &location]
        .iter()
        .map(|p| ((*p).clone(), file_digest(p)))
        .collect();
    let generation_before = f.session(&s1)["generation"].clone();
    let backup = t.join("backup");
    let from = t.join("user");
    let to = t.join("disk");
    let args = |dry: bool| {
        let mut a = vec![
            "root".to_string(),
            "migrate".into(),
            "--from".into(),
            from.to_string_lossy().into_owned(),
            "--to".into(),
            to.to_string_lossy().into_owned(),
            "--backup".into(),
            backup.to_string_lossy().into_owned(),
            // The test tmpfs cannot reflink.
            "--allow-full-copy".into(),
        ];
        if dry {
            a.push("--dry-run".into());
        }
        a
    };
    let run = |a: Vec<String>| {
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        f.run(&refs)
    };

    // --dry-run: the full plan, nothing written.
    let out = run(args(true));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("# root migrate plan"));
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    let plan = plan.get("result").cloned().unwrap_or(plan);
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["entries"].as_array().unwrap().len(), 2, "{plan}");
    assert_eq!(
        plan["preconditions"]["P2_alias_proven"]["ok"], true,
        "{plan}"
    );
    assert_eq!(plan["preconditions"]["P6_backup"]["ok"], true, "{plan}");
    assert!(
        plan["claude"]["rekey"]
            .as_array()
            .unwrap()
            .contains(&json!(s1)),
        "{plan}"
    );
    assert!(
        plan["collisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["session"] == s2.as_str()),
        "{plan}"
    );
    assert!(
        plan["left_behind"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["session"] == s2.as_str())
    );
    assert_eq!(plan["links"].as_array().unwrap().len(), 1, "{plan}");
    let existing = plan["existing_target_keys"].as_array().unwrap();
    assert!(
        existing.iter().any(|k| k["key"] == key(&new)
            && k["entries"]
                .as_array()
                .unwrap()
                .contains(&json!("someone-else.jsonl"))),
        "{plan}"
    );
    assert!(
        plan["files_to_rewrite"].as_array().unwrap().len() >= 3,
        "{plan}"
    );
    assert!(!backup.exists(), "a dry run writes nothing");
    for (p, d) in &digests {
        assert_eq!(&file_digest(p), d);
    }
    assert!(
        projects
            .join(key(&old))
            .join(format!("{n1}.jsonl"))
            .exists()
    );

    // A non-empty backup directory is refused.
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("x"), "x").unwrap();
    let refused = run(args(false));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("BACKUP_NOT_EMPTY"));
    std::fs::remove_file(backup.join("x")).unwrap();

    // The real run.
    let out = run(args(false));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    let result = result.get("result").cloned().unwrap_or(result);
    assert_eq!(result["state"], "complete", "{result}");
    // S1 moved to the new key; the retained copy exists, verified, under the backup.
    assert!(
        !projects
            .join(key(&old))
            .join(format!("{n1}.jsonl"))
            .exists()
    );
    assert!(target.join(format!("{n1}.jsonl")).is_file());
    assert!(target.join(&n1).join("tool-results/r.txt").is_file());
    assert_eq!(read(&target.join("memory/MEMORY.md")), "old memory");
    assert_eq!(
        read(&projects.join(key(&old)).join("memory/MEMORY.md")),
        "old memory",
        "memory is copied, never moved"
    );
    assert_eq!(read(&target.join("someone-else.jsonl")), "{}\n");
    assert_eq!(read(&target.join("memory/theirs.md")), "theirs");
    assert_eq!(
        f.catalog_directory(&s1).as_deref(),
        Some(&*new.to_string_lossy())
    );
    // S2 stayed on its old path; nothing of it moved.
    assert_eq!(
        f.catalog_directory(&s2).as_deref(),
        Some(&*other.to_string_lossy())
    );
    assert!(
        projects
            .join(key(&other))
            .join(format!("{n2}.jsonl"))
            .exists()
    );
    // Registry records and the link use the new spelling.
    assert!(read(&owner).contains(&*t.join("disk/appdata/state").to_string_lossy()));
    assert!(read(&location).contains(&*new.to_string_lossy()));
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        t.join("disk/appdata/state/shared/claude/projects")
    );
    let status = f.json(&["root", "status"]);
    assert_eq!(status["layout_version"], 3, "{status}");
    assert_eq!(status["gate"], "pass", "{status}");
    let rec = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == s1.as_str())
        .unwrap()
        .clone();
    assert_eq!(rec["state"], "rekeyed", "{rec}");
    let retained = rec["retained"].as_array().unwrap();
    assert!(!retained.is_empty());
    for r in retained {
        let copy = PathBuf::from(r["copy"].as_str().unwrap());
        assert!(copy.starts_with(&backup) && copy.exists(), "{r}");
        assert_eq!(r["verified"], true);
    }
    let left: Vec<_> = status["left_behind"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert!(left.contains(&json!(s2)), "{status}");
    assert!(
        status["collisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["session"] == s2.as_str())
    );
    // A second run is a no-op.
    let again = run(args(false));
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(String::from_utf8_lossy(&again.stdout).contains("already_active"));
    // The root opens by either spelling as one root; api version reports both.
    let v = f.api(&["version"]);
    assert_eq!(v["root"], json!(f.paths.root));
    // Finalize keeps every retained copy and removes nothing that is not empty.
    let fin = f.json(&["root", "finalize", "--accept-unresumed", "--dry-run"]);
    assert_eq!(fin["memory_removed"], false);
    assert!(
        fin["retained_copies"]
            .as_str()
            .unwrap()
            .contains("never deleted")
    );

    // Whole-root rollback: layout version 2 and the original records.
    let rb = f.json(&["root", "rollback"]);
    assert_eq!(rb["rolled_back"], true, "{rb}");
    assert!(rb["problems"].as_array().unwrap().is_empty(), "{rb}");
    for (p, d) in &digests {
        assert_eq!(&file_digest(p), d, "{} restored byte for byte", p.display());
    }
    assert_eq!(std::fs::read_link(&link).unwrap(), store_old);
    assert!(
        projects
            .join(key(&old))
            .join(format!("{n1}.jsonl"))
            .is_file()
    );
    assert!(!target.join(format!("{n1}.jsonl")).exists());
    assert_eq!(
        f.catalog_directory(&s1).as_deref(),
        Some(&*old.to_string_lossy())
    );
    let status = f.json(&["root", "status"]);
    assert_eq!(status["layout_version"], 2, "{status}");
    for r in retained {
        assert!(
            PathBuf::from(r["copy"].as_str().unwrap()).exists(),
            "rollback keeps retained copies"
        );
    }
    // The original generation is back (directory and revision restored exactly).
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(f.paths.root.join("shared/claude/projects"), &link).unwrap();
    f.start_daemon().await;
    assert_eq!(f.session(&s1)["generation"], generation_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_r_is_blocked_by_an_operation_in_flight_and_needs_the_supervisor_stopped() {
    let mut f = Fixture::new().await;
    let t = f.tmp.path().to_path_buf();
    let args = [
        "root".to_string(),
        "migrate".into(),
        "--from".into(),
        t.join("user").to_string_lossy().into_owned(),
        "--to".into(),
        t.join("disk").to_string_lossy().into_owned(),
        "--backup".into(),
        t.join("bk").to_string_lossy().into_owned(),
    ];
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = f.run(&refs);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("DAEMON_RUNNING"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    f.stop_daemon();
    // Final journals never block P4: an abandoned launch and a phase-less deletion
    // tombstone (`deletions/<thread>.json`). The non-terminal switch below is then
    // the only blocker named.
    let launches = f.paths.root.join("operations/launches");
    std::fs::create_dir_all(&launches).unwrap();
    let abandoned = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        launches.join(format!("{abandoned}.json")),
        serde_json::to_vec(
            &json!({"version":1,"phase":"abandoned","request":{"request_id":abandoned}}),
        )
        .unwrap(),
    )
    .unwrap();
    let deletions = f.paths.root.join("operations/deletions");
    std::fs::create_dir_all(&deletions).unwrap();
    let tomb = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        deletions.join(format!("{tomb}.json")),
        serde_json::to_vec(&json!({"confirmation":"x","copies":[],"thread_ids":[tomb]})).unwrap(),
    )
    .unwrap();
    // A non-terminal Claude switch journal blocks Tier R (P4).
    let dir = f.paths.root.join("operations/claude-switches");
    std::fs::create_dir_all(&dir).unwrap();
    let op = uuid::Uuid::new_v4().to_string();
    let rec = json!({"schema_version":1,"id":op,"kind":"account","session":{"id":uuid::Uuid::new_v4(),"name":"n","provider":"claude","account":"a","conversation_id":uuid::Uuid::new_v4(),"revision":1,"attachment":null},
        "session_id":uuid::Uuid::new_v4(),"conversation_id":uuid::Uuid::new_v4(),"conversation_ids":[],"expected_generation":"g","accept_risks":false,
        "source_account":"a","target_account":"b","source_home":"/x","target_home":"/y","directory":null,"pane":null,"process":null,"permissions":null,
        "entries":null,"phase":"copying","detail":"","uncertain_from":null,"resume":"pending","generation_after":null,"created_at":1,"updated_at":4102444800i64});
    std::fs::write(
        dir.join(format!("{op}.json")),
        serde_json::to_vec(&rec).unwrap(),
    )
    .unwrap();
    let out = f.run(&refs);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("ROOT_MIGRATION_BLOCKED") && err.contains(&op),
        "{err}"
    );
    assert!(!err.contains(&abandoned) && !err.contains(&tomb), "{err}");
    assert!(!t.join("bk").exists());
    let layout: Value =
        serde_json::from_slice(&std::fs::read(f.paths.root.join("layout.json")).unwrap()).unwrap();
    assert_eq!(layout["version"], 2);
    // Without the in-flight journal, a backup that cannot reflink (tmpfs here)
    // is refused unless --allow-full-copy accepts full copies.
    std::fs::remove_file(dir.join(format!("{op}.json"))).unwrap();
    let out = f.run(&refs);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("BACKUP_REFLINK_UNSUPPORTED"), "{err}");
    let layout: Value =
        serde_json::from_slice(&std::fs::read(f.paths.root.join("layout.json")).unwrap()).unwrap();
    assert_eq!(layout["version"], 2, "nothing changed");
}

/// Tier S with a running Claude session in a fake HERDR pane: stopped,
/// re-keyed, resumed in the same pane and verified; the F2 gate passes for it
/// after activation. Then a resume that starts a fresh conversation reverts to
/// `left_behind` with the old transcript byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_s_running_claude_verifies_and_a_fresh_conversation_reverts() {
    let mut f = Fixture::new().await;
    let t = f.tmp.path().to_path_buf();
    let old = f.old_project.clone();
    let new = f.new_project.clone();
    let projects = f.home().join("projects");
    let start = |f: &Fixture, id: &str| {
        let s = f.session(id);
        let out = f.run(&[
            "--permissions",
            "default",
            "api",
            "resume",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--herdr-session",
            "fixture",
            "--space",
            "w1",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let s = f.live_session(id);
            if s["availability"] == "running" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{id} did not start: {s}\nherdr: {}\nerr: {}",
                read(&f.tmp.path().join("herdr-state/calls")),
                std::fs::read_dir(f.tmp.path().join("herdr-state"))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".err"))
                    .map(|e| read(&e.path()))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let (s1, n1) = f.stopped_session(&old);
    start(&f, &s1);
    let old_pid = f.live_session(&s1)["pid"].as_u64().unwrap();
    // Tier R with the session running: it is deferred, not stopped.
    f.stop_daemon();
    let out = f.run(&[
        "root",
        "migrate",
        "--from",
        &t.join("user").to_string_lossy(),
        "--to",
        &t.join("disk").to_string_lossy(),
        "--backup",
        &t.join("bk").to_string_lossy(),
        "--allow-full-copy",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    f.start_daemon().await;
    let status = f.json(&["root", "status"]);
    let row = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == s1.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["state"], "deferred_running", "{status}");
    assert_eq!(status["gate"], "pass", "{status}");
    let gate = status["gate_detail"]["sessions"].as_array().unwrap();
    assert!(
        gate.iter().any(|g| g["session_id"] == s1.as_str()
            && g["running_before"] == true
            && g["running_now"] == true),
        "{status}"
    );
    assert!(
        std::fs::read_to_string(format!("/proc/{old_pid}/stat")).is_ok(),
        "never stopped by Tier R"
    );

    // Tier S: strict resume (finds the transcript only in key(new cwd)).
    std::fs::write(f.home().join("fake-resume-mode"), "strict").unwrap();
    let busy = f.run(&["root", "migrate", "--session", &s1]);
    assert!(String::from_utf8_lossy(&busy.stderr).contains("RISK_CONFIRMATION_REQUIRED"));
    let started = f.json(&["root", "migrate", "--session", &s1, "--accept-risks"]);
    assert_eq!(started["worker"], "started", "{started}");
    let mut last = Value::Null;
    wait_for("tier S outcome", || {
        last = f.json(&["root", "status"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == s1.as_str())
            .cloned()
            .unwrap();
        matches!(
            last["state"].as_str(),
            Some("verified" | "left_behind" | "uncertain" | "failed")
        )
    });
    assert_eq!(last["state"], "verified", "{last}");
    assert!(
        projects
            .join(key(&new))
            .join(format!("{n1}.jsonl"))
            .is_file()
    );
    assert!(
        !projects
            .join(key(&old))
            .join(format!("{n1}.jsonl"))
            .exists()
    );
    let launch = read(&f.home().join("fake-launch-resume"));
    assert!(
        launch.contains(&format!("cwd={}", new.display())),
        "{launch}"
    );
    assert!(launch.contains(&format!("session={n1}")), "{launch}");
    assert_eq!(
        f.catalog_directory(&s1).as_deref(),
        Some(&*new.to_string_lossy())
    );
    let gate = f.json(&["root", "status"]);
    assert_eq!(gate["gate"], "pass", "{gate}");

    // A second session whose resume starts a fresh conversation reverts.
    let (s2, n2) = f.stopped_session(&old);
    std::fs::write(f.home().join("fake-resume-mode"), "cross").unwrap();
    start(&f, &s2);
    let before = std::fs::read(projects.join(key(&old)).join(format!("{n2}.jsonl"))).unwrap();
    std::fs::write(f.home().join("fake-resume-mode"), "fresh").unwrap();
    let started = f.json(&["root", "migrate", "--session", &s2, "--accept-risks"]);
    assert_eq!(started["worker"], "started", "{started}");
    wait_for("revert outcome", || {
        last = f.json(&["root", "status"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == s2.as_str())
            .cloned()
            .unwrap();
        matches!(
            last["state"].as_str(),
            Some("verified" | "left_behind" | "uncertain" | "failed")
        )
    });
    assert!(
        matches!(last["state"].as_str(), Some("left_behind" | "uncertain")),
        "{last}"
    );
    assert!(
        last["reason"].as_str().unwrap_or_default().contains("V1") || last["state"] == "uncertain",
        "{last}"
    );
    assert_eq!(
        std::fs::read(projects.join(key(&old)).join(format!("{n2}.jsonl"))).unwrap()
            [..before.len()],
        before[..]
    );
    assert_eq!(
        f.catalog_directory(&s2).as_deref(),
        Some(&*old.to_string_lossy())
    );
    // Clean up the fake processes this test started.
    for id in [&s1, &s2] {
        if let Some(pid) = f.live_session(id)["pid"].as_u64() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    for p in std::fs::read_dir(t.join("herdr-state"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if p.file_name().to_string_lossy().starts_with("pid-")
            && let Ok(pid) = read(&p.path()).trim().parse::<i32>()
        {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

impl Fixture {
    /// Start a stopped session in a fake HERDR pane and wait until it runs.
    fn start_in_pane(&self, id: &str) {
        let s = self.session(id);
        let out = self.run(&[
            "--permissions",
            "default",
            "api",
            "resume",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--herdr-session",
            "fixture",
            "--space",
            "w1",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        wait_for("running", || {
            self.live_session(id)["availability"] == "running"
        });
    }
    fn tier_r(&self, fault: Option<&str>) -> Output {
        let t = self.tmp.path();
        let mut c = self.command(&[
            "root",
            "migrate",
            "--from",
            &t.join("user").to_string_lossy(),
            "--to",
            &t.join("disk").to_string_lossy(),
            "--backup",
            &t.join("bk").to_string_lossy(),
            "--allow-full-copy",
        ]);
        if let Some(f) = fault {
            c.env("SESSIONSATION_TEST_FAULT", f);
        }
        c.output().unwrap()
    }
    fn rec(&self, id: &str) -> Value {
        self.json(&["root", "status"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn settled(&self, id: &str) -> Value {
        let mut last = Value::Null;
        wait_for("tier S outcome", || {
            last = self.rec(id);
            last["phase"] == "complete"
                && matches!(
                    last["state"].as_str(),
                    Some("verified" | "left_behind" | "verify_failed")
                )
                || matches!(last["phase"].as_str(), Some("uncertain" | "failed"))
        });
        last
    }
    fn kill_fakes(&self, ids: &[&str]) {
        for id in ids {
            if let Some(pid) = self.live_session(id)["pid"].as_u64() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
        for p in std::fs::read_dir(self.tmp.path().join("herdr-state"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if p.file_name().to_string_lossy().starts_with("pid-")
                && let Ok(pid) = read(&p.path()).trim().parse::<i32>()
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

/// Review 2 B3 + S9: after the worker stopped the session, a failure after the
/// move (memory conflict, STALE_TARGET, I/O error) moves the files back,
/// restores the catalog and RESUMES the session on the old path (`left_behind`);
/// a worker that dies after the move is continued from its journal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_s_failures_after_the_move_resume_on_the_old_path_and_a_dead_worker_continues() {
    let mut f = Fixture::new().await;
    let old = f.old_project.clone();
    let new = f.new_project.clone();
    let projects = f.home().join("projects");
    std::fs::write(f.home().join("fake-resume-mode"), "cross").unwrap();
    let mut ids = vec![];
    for _ in 0..4 {
        let (id, native) = f.stopped_session(&old);
        f.start_in_pane(&id);
        ids.push((id, native));
    }
    f.stop_daemon();
    let out = f.tier_r(None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    f.start_daemon().await;
    for ((id, native), fault) in ids
        .iter()
        .zip(["memory_conflict", "stale_target", "io_error"])
    {
        let pid_before = f.live_session(id)["pid"].as_u64();
        let out = f
            .command(&["root", "migrate", "--session", id, "--accept-risks"])
            .env("SESSIONSATION_TEST_FAULT", fault)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{fault}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rec = f.settled(id);
        assert_eq!(rec["state"], "left_behind", "{fault}: {rec}");
        assert!(
            projects
                .join(key(&old))
                .join(format!("{native}.jsonl"))
                .is_file(),
            "{fault}"
        );
        assert!(
            !projects
                .join(key(&new))
                .join(format!("{native}.jsonl"))
                .exists(),
            "{fault}"
        );
        assert_eq!(
            f.catalog_directory(id).as_deref(),
            Some(&*old.to_string_lossy()),
            "{fault}"
        );
        // Resumed (a new process) on the old path.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let s = f.live_session(id);
            if s["availability"] == "running" && s["pid"].as_u64() != pid_before {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{fault}: not resumed: {s}\nrec: {rec}\nherdr: {}\nerr: {}",
                read(&f.tmp.path().join("herdr-state/calls")),
                std::fs::read_dir(f.tmp.path().join("herdr-state"))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".err"))
                    .map(|e| format!("{:?}: {}", e.file_name(), read(&e.path())))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    // A worker that dies right after the move is continued by a rerun.
    let (id, native) = &ids[3];
    let out = f
        .command(&["root", "migrate", "--session", id, "--accept-risks"])
        .env("SESSIONSATION_TEST_FAULT", "exit_after_moved")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for("worker death after the move", || {
        let r = f.rec(id);
        r["phase"] == "moved"
            && std::fs::OpenOptions::new()
                .write(true)
                .open(f.paths.root.join(format!(
                    "operations/root-migrations/session-{id}.worker.lock"
                )))
                .is_ok_and(|l| fs2::FileExt::try_lock_exclusive(&l).is_ok())
    });
    let again = f.json(&["root", "migrate", "--session", id, "--accept-risks"]);
    assert_eq!(again["continued"], true, "{again}");
    let rec = f.settled(id);
    assert_eq!(rec["state"], "verified", "{rec}");
    assert!(
        projects
            .join(key(&new))
            .join(format!("{native}.jsonl"))
            .is_file()
    );
    let refs: Vec<&str> = ids.iter().map(|(i, _)| i.as_str()).collect();
    f.kill_fakes(&refs);
}

/// Review 2 S9: a whole-root run that dies inside the Claude step with sessions
/// present, then `root rollback`: layout version 2, files and catalog agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_whole_root_crash_mid_step_with_sessions_rolls_back_consistently() {
    let mut f = Fixture::new().await;
    let old = f.old_project.clone();
    let new = f.new_project.clone();
    let projects = f.home().join("projects");
    let (s1, n1) = f.stopped_session(&old);
    let (s2, n2) = f.stopped_session(&old);
    f.stop_daemon();
    let out = f.tier_r(Some("exit_after_moved"));
    assert_eq!(
        out.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let layout: Value =
        serde_json::from_slice(&std::fs::read(f.paths.root.join("layout.json")).unwrap()).unwrap();
    assert_eq!(layout["version"], 3, "died after `mark`");
    let rb = f.json(&["root", "rollback"]);
    assert_eq!(rb["rolled_back"], true, "{rb}");
    let layout: Value =
        serde_json::from_slice(&std::fs::read(f.paths.root.join("layout.json")).unwrap()).unwrap();
    assert_eq!(layout["version"], 2);
    for (id, native) in [(&s1, &n1), (&s2, &n2)] {
        assert!(
            projects
                .join(key(&old))
                .join(format!("{native}.jsonl"))
                .is_file()
        );
        assert!(
            !projects
                .join(key(&new))
                .join(format!("{native}.jsonl"))
                .exists()
        );
        assert_eq!(
            f.catalog_directory(id).as_deref(),
            Some(&*old.to_string_lossy())
        );
    }
}

/// F2 with a real bind mount: needs a private user+mount namespace
/// (`unshare -Urm`), which CI containers and the check image do not allow.
/// Run manually: `cargo test --test root_migrate bind_namespace -- --ignored`
/// on a host with unprivileged user namespaces.
#[test]
#[ignore = "needs unshare -Urm (user + mount namespace) for a real bind mount; not available in the check container"]
fn bind_namespace_running_claude_is_still_live_after_activation() {
    if std::env::var_os("ROOT_MIGRATE_IN_NS").is_none() {
        let exe = std::env::current_exe().unwrap();
        let status = Command::new("unshare")
            .args(["-Urm", "--"])
            .arg(exe)
            .args([
                "--exact",
                "bind_namespace_running_claude_is_still_live_after_activation",
                "--ignored",
                "--nocapture",
            ])
            .env("ROOT_MIGRATE_IN_NS", "1")
            .status()
            .expect("unshare is unavailable");
        assert!(status.success());
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    std::fs::create_dir_all(t.join("disk/appdata")).unwrap();
    std::fs::create_dir_all(t.join("user/appdata")).unwrap();
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let (src, dst) = (c(&t.join("disk/appdata")), c(&t.join("user/appdata")));
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            dst.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    assert_eq!(
        rc,
        0,
        "bind mount failed inside the namespace: {}",
        std::io::Error::last_os_error()
    );
    let old_root = t.join("user/appdata/state");
    let paths = Paths::new(Some(old_root.clone())).unwrap();
    assert_eq!(
        paths.root, old_root,
        "a bind is not resolved by canonicalize (F1)"
    );
    let old_home = old_root.join("accounts/claude/a");
    std::fs::create_dir_all(old_home.join("sessions")).unwrap();
    let mut child = Command::new("sleep")
        .arg("30")
        .env("CLAUDE_CONFIG_DIR", &old_home)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let pid = child.id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_owned();
    std::fs::write(
        old_home.join("sessions").join(format!("{pid}.json")),
        serde_json::to_vec(&json!({"pid":pid,"procStart":start,"sessionId":uuid::Uuid::new_v4()}))
            .unwrap(),
    )
    .unwrap();
    sessionsation::alias::install(&[sessionsation::alias::Entry {
        from: t.join("user/appdata"),
        to: t.join("disk/appdata"),
        proof: "same_inode".into(),
    }]);
    let new_home = t.join("disk/appdata/state/accounts/claude/a");
    let live = sessionsation::provider::claude::live_sessions(&new_home).unwrap();
    // Codex: an app-server started with the old CODEX_HOME spelling.
    let codex_old = old_root.join("accounts/codex/c");
    std::fs::create_dir_all(&codex_old).unwrap();
    let mut codex = Command::new("sleep")
        .arg("30")
        .env("CODEX_HOME", &codex_old)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let codex_home = sessionsation::external::process_home(codex.id());
    let _ = child.kill();
    let _ = child.wait();
    let _ = codex.kill();
    let _ = codex.wait();
    assert_eq!(
        live.len(),
        1,
        "the old-spelling process is live for the new-spelling home"
    );
    assert_eq!(
        codex_home.unwrap(),
        t.join("disk/appdata/state/accounts/codex/c"),
        "the old CODEX_HOME spelling is the new-spelling account home"
    );
}
