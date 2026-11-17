use super::*;
use std::cell::RefCell;

thread_local! {
    static CRASH_AT: RefCell<Option<&'static str>> = const { RefCell::new(None) };
}
/// Simulated crash: the journal keeps the step just written, nothing after runs.
pub(super) fn crash_check(name: &str) -> Result<()> {
    ensure!(
        CRASH_AT.with(|c| *c.borrow()) != Some(name),
        "SIMULATED_CRASH at {name}"
    );
    Ok(())
}
fn crash_at(step: Option<&'static str>) {
    CRASH_AT.with(|c| *c.borrow_mut() = step);
}
thread_local! {
    static DIE_AT: RefCell<Option<&'static str>> = const { RefCell::new(None) };
}
/// Simulated process death: unwinds without running any error handling.
pub(super) fn die_check(name: &str) {
    if DIE_AT.with(|c| *c.borrow()) == Some(name) {
        panic!("SIMULATED_DEATH at {name}");
    }
}
fn die_at(step: Option<&'static str>) {
    DIE_AT.with(|c| *c.borrow_mut() = step);
}

fn write(p: &Path, text: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}
fn entries(t: &Path) -> Vec<Entry> {
    ["appdata", "live"]
        .iter()
        .map(|s| Entry {
            from: t.join("user").join(s),
            to: t.join("disk").join(s),
            proof: "test".into(),
        })
        .collect()
}

#[test]
fn project_key_encoding_matches_claude() {
    assert_eq!(
        keys::key("/mnt/user/live/projects/localica"),
        "-mnt-user-live-projects-localica"
    );
    assert_eq!(keys::key("/a/b.c d_e"), "-a-b-c-d-e");
    assert_eq!(keys::key("/x/Ünï"), "-x--n-");
}

// ------------------------------------------------------------------ memory

#[test]
fn memory_merge_covers_every_case_and_never_picks_a_winner() {
    let t = tempfile::tempdir().unwrap();
    let (src, dst, backup) = (
        t.path().join("src"),
        t.path().join("dst"),
        t.path().join("bk"),
    );
    write(&src.join("MEMORY.md"), "index v1");
    write(&src.join("only-src.md"), "a");
    write(&src.join("same.md"), "same");
    write(&dst.join("same.md"), "same");
    write(&dst.join("only-dst.md"), "kept");
    let mut base = BTreeMap::new();
    // Dry run computes the plan and writes nothing.
    let dry = keys::sync_memory(&src, &dst, &mut base, &backup, "k", true).unwrap();
    assert_eq!(dry.copied, ["MEMORY.md", "only-src.md"]);
    assert!(!dst.join("MEMORY.md").exists() && base.is_empty() && !backup.exists());
    // Missing -> copied; identical -> nothing; only in target -> left alone.
    let r = keys::sync_memory(&src, &dst, &mut base, &backup, "k", false).unwrap();
    assert_eq!(r.copied, ["MEMORY.md", "only-src.md"]);
    assert_eq!(r.identical, ["same.md"]);
    assert_eq!(r.only_in_target, ["only-dst.md"]);
    assert_eq!(
        fs::read_to_string(dst.join("MEMORY.md")).unwrap(),
        "index v1"
    );
    // Newer in src (dst still equals the base): fast-forward, the base version saved.
    write(&src.join("MEMORY.md"), "index v2");
    let r = keys::sync_memory(&src, &dst, &mut base, &backup, "k", false).unwrap();
    assert_eq!(r.fast_forwarded, ["MEMORY.md"]);
    assert_eq!(
        fs::read_to_string(dst.join("MEMORY.md")).unwrap(),
        "index v2"
    );
    let versions: Vec<_> = fs::read_dir(backup.join("memory-versions/k"))
        .unwrap()
        .collect();
    assert_eq!(versions.len(), 1);
    // Newer in dst (src still equals the base): dst kept.
    write(&dst.join("only-src.md"), "edited on the new side");
    let r = keys::sync_memory(&src, &dst, &mut base, &backup, "k", false).unwrap();
    assert_eq!(r.kept_newer, ["only-src.md"]);
    assert_eq!(
        fs::read_to_string(dst.join("only-src.md")).unwrap(),
        "edited on the new side"
    );
    // Both changed: conflict; both files unchanged in place, both copied aside.
    write(&src.join("MEMORY.md"), "old side edit");
    write(&dst.join("MEMORY.md"), "new side edit");
    let r = keys::sync_memory(&src, &dst, &mut base, &backup, "k", false).unwrap();
    assert_eq!(r.conflicts, ["MEMORY.md"]);
    assert!(!r.clean());
    assert_eq!(
        fs::read_to_string(src.join("MEMORY.md")).unwrap(),
        "old side edit"
    );
    assert_eq!(
        fs::read_to_string(dst.join("MEMORY.md")).unwrap(),
        "new side edit"
    );
    let c = backup.join("memory-conflicts/k");
    assert_eq!(
        fs::read_to_string(c.join("MEMORY.md.source")).unwrap(),
        "old side edit"
    );
    assert_eq!(
        fs::read_to_string(c.join("MEMORY.md.target")).unwrap(),
        "new side edit"
    );
    // Made identical by a person: the retry passes.
    write(&src.join("MEMORY.md"), "merged");
    write(&dst.join("MEMORY.md"), "merged");
    assert!(
        keys::sync_memory(&src, &dst, &mut base, &backup, "k", false)
            .unwrap()
            .clean()
    );
}

#[test]
fn a_first_sync_into_an_existing_target_with_a_differing_file_is_a_conflict() {
    let t = tempfile::tempdir().unwrap();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    write(&src.join("MEMORY.md"), "old key");
    write(
        &dst.join("MEMORY.md"),
        "written by a session already on the new path",
    );
    let mut base = BTreeMap::new();
    let r = keys::sync_memory(&src, &dst, &mut base, &t.path().join("bk"), "k", false).unwrap();
    assert_eq!(r.conflicts, ["MEMORY.md"]);
    assert_eq!(
        fs::read_to_string(dst.join("MEMORY.md")).unwrap(),
        "written by a session already on the new path"
    );
}

// --------------------------------------------------------------- moves

#[test]
fn moves_add_names_only_and_never_overwrite() {
    let t = tempfile::tempdir().unwrap();
    let projects = t.path().join("projects");
    let id = uuid::Uuid::new_v4().to_string();
    write(
        &projects.join("-old").join(format!("{id}.jsonl")),
        "transcript\n",
    );
    write(
        &projects.join("-old").join(&id).join("tool-results/r"),
        "side",
    );
    write(&projects.join("-new/other.jsonl"), "someone else");
    write(&projects.join("-new/memory/theirs.md"), "theirs");
    let found = keys::locate(&projects, std::slice::from_ref(&id)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, "-old");
    assert!(found[0].sidecar.is_some());
    assert!(keys::collisions(&projects, &found, "-new").is_empty());
    let before = crate::external::archiving::native::tree_digest(&projects.join("-new")).unwrap();
    // Retain first: copy + verify outside projects/.
    let retained = keys::retain(&found, &t.path().join("bk"), true).unwrap();
    assert_eq!(retained.len(), 2);
    assert!(retained.iter().all(|r| r.verified && r.copy.exists()));
    let mut journaled: Vec<Vec<String>> = vec![];
    let moved = keys::move_names(&projects, &retained, "-new", |m| {
        journaled.push(m.iter().map(|x| x.name.clone()).collect());
        Ok(())
    })
    .unwrap();
    assert_eq!(moved.len(), 2);
    // Each rename is journaled before (intent) and after it.
    assert_eq!(journaled.len(), 4, "{journaled:?}");
    assert_eq!(journaled[0].len(), 1);
    assert!(projects.join("-new").join(format!("{id}.jsonl")).is_file());
    assert_eq!(
        fs::read_to_string(projects.join("-new/other.jsonl")).unwrap(),
        "someone else"
    );
    // Moving back restores the original layout exactly.
    keys::move_back(&moved).unwrap();
    assert_eq!(
        crate::external::archiving::native::tree_digest(&projects.join("-new")).unwrap(),
        before
    );
    assert!(projects.join("-old").join(format!("{id}.jsonl")).is_file());
    // A pre-existing same name in the target is a collision (even identical content).
    write(&projects.join("-new").join(&id).join("x"), "occupant");
    let found = keys::locate(&projects, std::slice::from_ref(&id)).unwrap();
    assert_eq!(keys::collisions(&projects, &found, "-new").len(), 1);
    // A name appearing between preflight and rename is not overwritten; nothing stays moved.
    let retained = keys::retain(&found, &t.path().join("bk2"), true).unwrap();
    let e = keys::move_names(&projects, &retained, "-new", |_| Ok(()))
        .unwrap_err()
        .to_string();
    assert!(e.contains("ROOT_REKEY_CONFLICT"), "{e}");
    assert!(projects.join("-old").join(format!("{id}.jsonl")).is_file());
    assert_eq!(
        fs::read_to_string(projects.join("-new").join(&id).join("x")).unwrap(),
        "occupant"
    );
    // Two transcripts for one id: conflict.
    write(&projects.join("-third").join(format!("{id}.jsonl")), "dup");
    let e = keys::locate(&projects, &[id]).unwrap_err().to_string();
    assert!(e.contains("ROOT_REKEY_CONFLICT"), "{e}");
}

// ------------------------------------------------------------ .claude.json

#[test]
fn claude_json_entries_are_cloned_byte_preserving_and_removed_on_rollback() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let old = t.path().join("user/live/p").to_string_lossy().into_owned();
    let new = t.path().join("disk/live/p").to_string_lossy().into_owned();
    let other = "/elsewhere".to_string();
    let text = format!(
        "{{\n  \"numStartups\": 3,\n  \"projects\": {{\n    {}: {{\n      \"allowedTools\": [\"x\"],\n      \"hasTrustDialogAccepted\": true\n    }},\n    {}: {{}}\n  }},\n  \"z\": 1\n}}\n",
        serde_json::to_string(&old).unwrap(),
        serde_json::to_string(&other).unwrap()
    );
    fs::write(home.join(".claude.json"), &text).unwrap();
    let alias = Alias::new(entries(t.path()));
    let dry = keys::clone_project_entries(&home, &alias, None, true).unwrap();
    assert_eq!(dry.len(), 1);
    assert_eq!(fs::read_to_string(home.join(".claude.json")).unwrap(), text);
    let added = keys::clone_project_entries(&home, &alias, None, false).unwrap();
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].0, new);
    let after = fs::read_to_string(home.join(".claude.json")).unwrap();
    let v: Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["projects"][&new], v["projects"][&old]);
    assert_eq!(v["numStartups"], 3);
    // Idempotent: the key exists now.
    assert!(
        keys::clone_project_entries(&home, &alias, None, false)
            .unwrap()
            .is_empty()
    );
    // Rollback removes only the unchanged clone; the file returns to its bytes.
    assert_eq!(keys::remove_project_entries(&home, &added).unwrap(), [new]);
    assert_eq!(fs::read_to_string(home.join(".claude.json")).unwrap(), text);
}

// ----------------------------------------------------- comparison sites (R3)

/// The child's environment is visible once it has executed.
fn wait_env(pid: u32, key: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !claude::environ(pid).is_some_and(|e| e.iter().any(|(k, _)| k == key)) {
        assert!(
            std::time::Instant::now() < deadline,
            "child {pid} has no {key}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
/// F2: a Claude process started with the old spelling of its home (a bind,
/// not a symlink: two different paths) is a live session of the new spelling
/// only through the alias.
#[test]
fn a_process_with_the_old_home_spelling_is_live_only_through_the_alias() {
    let t = tempfile::tempdir().unwrap();
    let new_home = t.path().join("disk/appdata/root/accounts/claude/a");
    let old_home = t.path().join("user/appdata/root/accounts/claude/a");
    fs::create_dir_all(new_home.join("sessions")).unwrap();
    fs::create_dir_all(&old_home).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .env("CLAUDE_CONFIG_DIR", &old_home)
        .spawn()
        .unwrap();
    let pid = child.id();
    wait_env(pid, "CLAUDE_CONFIG_DIR");
    let (_, start) = claude::proc_start(pid).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    fs::write(
        new_home.join("sessions").join(format!("{pid}.json")),
        serde_json::to_vec(&json!({"pid":pid,"procStart":start.to_string(),"sessionId":session,"cwd":"/x","status":"idle"})).unwrap(),
    )
    .unwrap();
    assert!(
        claude::live_sessions(&new_home).unwrap().is_empty(),
        "without the alias the spellings differ"
    );
    let e = entries(t.path());
    alias::install(&e);
    let env_home = claude::environ(pid)
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| v);
    assert_eq!(env_home.as_deref(), Some(&*old_home.to_string_lossy()));
    assert_eq!(
        alias::canonical(&old_home).unwrap(),
        alias::canonical(&new_home).unwrap()
    );
    let live = claude::live_sessions(&new_home).unwrap();
    // Codex: the app-server's CODEX_HOME (old spelling) is the new-spelling home.
    let codex_old = t.path().join("user/appdata/root/accounts/codex/c");
    let codex_new = t.path().join("disk/appdata/root/accounts/codex/c");
    fs::create_dir_all(&codex_old).unwrap();
    fs::create_dir_all(&codex_new).unwrap();
    let mut codex = std::process::Command::new("sleep")
        .arg("30")
        .env("CODEX_HOME", &codex_old)
        .spawn()
        .unwrap();
    wait_env(codex.id(), "CODEX_HOME");
    let codex_home = crate::external::process_home(codex.id());
    alias::uninstall(&e);
    let _ = child.kill();
    let _ = child.wait();
    let _ = codex.kill();
    let _ = codex.wait();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].session_id, session);
    assert_eq!(codex_home.unwrap(), codex_new);
}

#[test]
fn same_home_resolve_path_and_trusted_link_accept_both_spellings() {
    let t = tempfile::tempdir().unwrap();
    let root_new = t.path().join("disk/appdata/root");
    let root_old = t.path().join("user/appdata/root");
    let home_new = layout::target_home(&root_new, ProviderId::Claude, "a");
    let home_old = layout::target_home(&root_old, ProviderId::Claude, "a");
    fs::create_dir_all(&home_new).unwrap();
    fs::create_dir_all(&home_old).unwrap();
    crate::shared_state::enable(&root_new).unwrap();
    let store_new = crate::shared_state::target(&root_new, "projects");
    crate::paths::private_dir(&store_new).unwrap();
    // A link whose stored target is the old spelling of the store (as on the live root)
    // resolves through a fake "bind": the old store path is a symlink to the new one.
    let store_old = crate::shared_state::target(&root_old, "projects");
    fs::create_dir_all(store_old.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store_new, &store_old).unwrap();
    std::os::unix::fs::symlink(&store_old, home_new.join("projects")).unwrap();
    let foreign = t.path().join("foreign");
    fs::create_dir_all(&foreign).unwrap();
    std::os::unix::fs::symlink(&foreign, home_new.join("todos")).unwrap();
    assert!(!crate::shared_state::trusted_link(
        &home_new.join("projects")
    ));
    assert!(!layout::same_home(&home_old, &home_new));
    let e = entries(t.path());
    alias::install(&e);
    let old_spelling = crate::shared_state::trusted_link(&home_new.join("projects"));
    let foreign_trusted = crate::shared_state::trusted_link(&home_new.join("todos"));
    // New-spelling target.
    fs::remove_file(home_new.join("projects")).unwrap();
    std::os::unix::fs::symlink(&store_new, home_new.join("projects")).unwrap();
    let new_spelling = crate::shared_state::trusted_link(&home_new.join("projects"));
    let same = layout::same_home(&home_old, &home_new);
    let resolved = layout::resolve_path(&home_old.join("not/yet/there"));
    alias::uninstall(&e);
    assert!(
        old_spelling,
        "old-spelling target is trusted through the alias"
    );
    assert!(new_spelling);
    assert!(!foreign_trusted, "a foreign target is never trusted");
    assert!(same);
    assert_eq!(resolved, home_new.join("not/yet/there"));
}

#[test]
fn alias_proof_requires_one_tree_and_refuses_symlinks_and_other_branches() {
    let t = tempfile::tempdir().unwrap();
    let mnt = t.path().join("mnt");
    fs::create_dir_all(mnt.join("user/appdata")).unwrap();
    // Same inode (here: the very same directory) is proven without writing.
    assert_eq!(
        prove(&mnt.join("user/appdata"), &mnt.join("user/appdata"), true).unwrap(),
        "same_inode"
    );
    fs::create_dir_all(mnt.join("disk1/appdata")).unwrap();
    // Two different directories: a dry run cannot prove them; a probe file shows they differ.
    assert_eq!(
        prove(&mnt.join("user/appdata"), &mnt.join("disk1/appdata"), true).unwrap_err(),
        "probe_required"
    );
    assert_eq!(
        prove(&mnt.join("user/appdata"), &mnt.join("disk1/appdata"), false).unwrap_err(),
        "probe_mismatch"
    );
    // A symlinked spelling is refused (canonicalize would hide the alias).
    std::os::unix::fs::symlink(mnt.join("disk1/appdata"), mnt.join("user/live")).unwrap();
    fs::create_dir_all(mnt.join("disk1/live")).unwrap();
    if !test_symlink_proof() {
        assert_eq!(
            prove(&mnt.join("user/live"), &mnt.join("disk1/live"), true).unwrap_err(),
            "symlink"
        );
    }
    // An empty share directory on another array disk is not divergence.
    fs::create_dir_all(mnt.join("disk2/appdata")).unwrap();
    assert_eq!(
        prove(&mnt.join("user/appdata"), &mnt.join("disk1/appdata"), true).unwrap_err(),
        "probe_required"
    );
    // Content on another array disk makes it unprovable.
    write(&mnt.join("disk2/appdata/x/file"), "divergent");
    assert!(
        prove(&mnt.join("user/appdata"), &mnt.join("disk1/appdata"), false)
            .unwrap_err()
            .starts_with("other_branch:")
    );
    // user0 (array view), remotes, addons and plain non-mount directories are never branches.
    fs::rename(mnt.join("disk2"), mnt.join("remotes")).unwrap();
    for n in ["user0", "addons", "notapool"] {
        write(&mnt.join(n).join("appdata/x"), "x");
    }
    assert_eq!(
        prove(&mnt.join("user/appdata"), &mnt.join("disk1/appdata"), true).unwrap_err(),
        "probe_required"
    );
    let branches = other_branches(
        &mnt,
        std::ffi::OsStr::new("user"),
        std::ffi::OsStr::new("disk1"),
    );
    assert!(branches.is_empty(), "{branches:?}");
}

// ------------------------------------------------- B1-B3 and should-fix items

struct One {
    t: tempfile::TempDir,
    paths: Paths,
    home: PathBuf,
    old: String,
    new: String,
    conv: String,
    s: Session,
    rec: SessionRec,
}
impl One {
    fn projects(&self) -> PathBuf {
        self.home.join("projects")
    }
    fn old_transcript(&self) -> PathBuf {
        self.projects()
            .join(keys::key(&self.old))
            .join(format!("{}.jsonl", self.conv))
    }
    fn new_transcript(&self) -> PathBuf {
        self.projects()
            .join(keys::key(&self.new))
            .join(format!("{}.jsonl", self.conv))
    }
    fn dir_now(&self) -> Option<String> {
        catalog::find(&self.paths, &self.s.id).unwrap().directory
    }
    fn ctx(&self) -> session::Ctx<'_> {
        session::Ctx::new(&self.paths, &self.t.path().join("bk"), true)
    }
}
/// One stopped Claude session in a plain home (not an enrolled account), its
/// directory on the "old" side and a distinct "new" directory.
fn one() -> One {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let home = t.path().join("home");
    let old = t.path().join("user/live/p");
    let new = t.path().join("disk/live/p");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    let (old, new) = (
        old.to_string_lossy().into_owned(),
        new.to_string_lossy().into_owned(),
    );
    let conv = uuid::Uuid::new_v4().to_string();
    let k = home.join("projects").join(keys::key(&old));
    write(&k.join(format!("{conv}.jsonl")), "{\"type\":\"user\"}\n");
    write(&k.join(&conv).join("tool-results/r"), "side");
    write(&k.join("memory/MEMORY.md"), "mem");
    let s = catalog::record_provider_launch(
        &paths,
        ProviderId::Claude,
        "a",
        &conv,
        "n",
        Some(&old),
        None,
    )
    .unwrap();
    let mut rec = new_rec(&s, "run");
    rec.home = Some(home.clone());
    rec.from = Some(old.clone());
    rec.to = Some(new.clone());
    rec.state = "rekeying".into();
    rec.phase = "admitted".into();
    One {
        t,
        paths,
        home,
        old,
        new,
        conv,
        s,
        rec,
    }
}

#[test]
fn b3b_an_error_after_the_move_moves_the_names_back_first() {
    let mut f = one();
    let mut rec = f.rec.clone();
    crash_at(Some("memory_io"));
    let e = session::rekey_stopped(&mut f.ctx(), &mut rec, &f.s).unwrap_err();
    crash_at(None);
    assert!(e.to_string().contains("SIMULATED_CRASH"), "{e}");
    assert!(f.old_transcript().is_file(), "moved back before reporting");
    assert!(!f.new_transcript().exists());
    assert_eq!(f.dir_now().as_deref(), Some(f.old.as_str()));
    session::abandon_rekey(&f.paths, &mut rec, &e).unwrap();
    let saved = read_session(&f.paths, &f.s.id).unwrap();
    assert_eq!(
        (saved.state.as_str(), saved.phase.as_str()),
        ("left_behind", "complete")
    );
    assert!(saved.moved.is_empty());
    // The retained copy stays.
    assert!(saved.retained.iter().all(|r| r.copy.exists()));
    f.rec = saved;
}

#[test]
fn b3e_death_after_the_catalog_switch_is_reconciled_from_the_journal() {
    let f = one();
    let mut rec = f.rec.clone();
    save_session(&f.paths, &mut rec).unwrap();
    die_at(Some("catalog_switched"));
    let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ctx = f.ctx();
        let _ = session::rekey_stopped(&mut ctx, &mut rec, &f.s);
    }));
    die_at(None);
    assert!(died.is_err());
    // The journal says `moved`; the catalog already names the new directory.
    let journal = read_session(&f.paths, &f.s.id).unwrap();
    assert_eq!(journal.phase, "moved");
    assert_eq!(f.dir_now().as_deref(), Some(f.new.as_str()));
    assert!(
        crate::lifecycle::pending(&f.paths, &f.conv, None)
            .unwrap()
            .is_some(),
        "an unfinished re-key blocks other operations"
    );
    // Tier R rerun: the plan sees the new directory (not_applicable), the
    // per-session journal is reconciled first.
    let mut run = Run {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        backup: f.t.path().join("bk"),
        allow_full_copy: true,
        ..Default::default()
    };
    let plan = build_plan(&f.paths, &Alias::default(), &Ok(vec![])).unwrap();
    claude_step(&f.paths, &mut run, &Alias::default(), &plan).unwrap();
    let done = read_session(&f.paths, &f.s.id).unwrap();
    assert_eq!(
        (done.state.as_str(), done.phase.as_str()),
        ("rekeyed", "complete")
    );
    assert!(f.new_transcript().is_file());
    assert!(
        crate::lifecycle::pending(&f.paths, &f.conv, None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn b3a_rollback_moves_files_back_whatever_the_reported_state() {
    let f = one();
    let mut rec = f.rec.clone();
    session::rekey_stopped(&mut f.ctx(), &mut rec, &f.s).unwrap();
    assert!(f.new_transcript().is_file());
    rec.state = "uncertain".into();
    rec.phase = "uncertain".into();
    session::rollback_stopped(&f.paths, &mut rec).unwrap();
    assert!(f.old_transcript().is_file());
    assert!(!f.new_transcript().exists());
    assert_eq!(f.dir_now().as_deref(), Some(f.old.as_str()));
}

#[test]
fn b3c_a_retry_over_moved_files_is_refused() {
    let f = one();
    let mut rec = f.rec.clone();
    die_at(Some("catalog_switched"));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ctx = f.ctx();
        let _ = session::rekey_stopped(&mut ctx, &mut rec, &f.s);
    }));
    die_at(None);
    let mut journal = read_session(&f.paths, &f.s.id).unwrap();
    journal.state = "left_behind".into();
    journal.phase = "complete".into();
    let e = session::retry_allowed(&journal).unwrap_err().to_string();
    assert!(e.contains("ROOT_REKEY_CONFLICT"), "{e}");
    let clean = new_rec(&f.s, "run");
    session::retry_allowed(&clean).unwrap();
}

#[test]
fn b3d_whole_root_rollback_undoes_a_session_left_mid_rekey() {
    let f = one();
    let layout_bytes = fs::read(f.paths.root.join(layout::LAYOUT_FILE)).unwrap();
    let mut run = Run {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        backup: f.t.path().join("bk"),
        step: "activate".into(),
        state: "running".into(),
        layout_before: serde_json::to_value(layout::read_record(&f.paths.root).unwrap().unwrap())
            .unwrap(),
        layout_before_sha256: sha(&layout_bytes),
        created_at: now(),
        ..Default::default()
    };
    save_run(&f.paths, &mut run).unwrap();
    let mut rec = f.rec.clone();
    rec.run = run.id.clone();
    die_at(Some("catalog_switched"));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ctx = f.ctx();
        let _ = session::rekey_stopped(&mut ctx, &mut rec, &f.s);
    }));
    die_at(None);
    assert_eq!(read_session(&f.paths, &f.s.id).unwrap().state, "rekeying");
    let r = rollback(&f.paths, false).unwrap();
    assert_eq!(r["rolled_back"], true, "{r}");
    assert!(f.old_transcript().is_file());
    assert!(!f.new_transcript().exists());
    assert_eq!(f.dir_now().as_deref(), Some(f.old.as_str()));
}

#[test]
fn b1_quiet_check_fails_closed() {
    let f = one();
    let rec = f.rec.clone();
    let found = keys::locate(&f.projects(), &rec.conversation_ids).unwrap();
    // A process naming the conversation on its command line.
    let mut child = std::process::Command::new("sh")
        .args(["-c", "sleep 30", "sh", &f.conv])
        .spawn()
        .unwrap();
    wait_cmdline(child.id(), &f.conv);
    let e = session::ensure_quiet_pub(&f.paths, &rec, &found)
        .unwrap_err()
        .to_string();
    let _ = child.kill();
    let _ = child.wait();
    assert!(e.contains("SESSION_RUNNING"), "{e}");
    // A writer that opened the transcript through a symlinked `projects`.
    let link_home = f.t.path().join("linkhome");
    fs::create_dir_all(&link_home).unwrap();
    std::os::unix::fs::symlink(f.projects(), link_home.join("projects")).unwrap();
    let via_link = link_home
        .join("projects")
        .join(keys::key(&f.old))
        .join(format!("{}.jsonl", f.conv));
    let mut writer = std::process::Command::new("sh")
        .args(["-c", "exec 3>>\"$1\"; sleep 30", "sh"])
        .arg(&via_link)
        .spawn()
        .unwrap();
    let fd_open = || {
        fs::read_dir(format!("/proc/{}/fd", writer.id()))
            .map(|d| d.count() > 3)
            .unwrap_or(false)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !fd_open() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut linked = rec.clone();
    linked.home = Some(link_home.clone());
    let found = keys::locate(&link_home.join("projects"), &rec.conversation_ids).unwrap();
    let e = session::ensure_quiet_pub(&f.paths, &linked, &found)
        .unwrap_err()
        .to_string();
    let _ = writer.kill();
    let _ = writer.wait();
    assert!(e.contains("HISTORY_IN_USE"), "{e}");
    // An unreadable live registry is a failed probe, never "stopped".
    let broken = f.t.path().join("broken");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("sessions"), "not a directory").unwrap();
    let mut b = rec.clone();
    b.home = Some(broken);
    let e = session::ensure_quiet_pub(&f.paths, &b, &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("probe_failed"), "{e}");
}
fn wait_cmdline(pid: u32, needle: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|c| c.split(|b| *b == 0).any(|a| a == needle.as_bytes()))
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn b1_a_codex_thread_is_never_planned_rekey_when_the_inventory_failed() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let thread = uuid::Uuid::new_v4().to_string();
    catalog::record_launch(&paths, "c", &thread, "n", None).unwrap();
    let old = t.path().join("user/live/q");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(t.path().join("disk/live/q")).unwrap();
    crate::external::relocation::save_location(&paths, &thread, &old).unwrap();
    let alias = Alias::new(entries(t.path()));
    let plan = build_plan(&paths, &alias, &Err("app-server unreachable".into())).unwrap();
    let c = plan.codex.iter().find(|c| c.thread == thread).unwrap();
    assert_eq!(c.state, "left_behind");
    assert!(
        c.reason.as_deref().unwrap().starts_with("probe_failed"),
        "{:?}",
        c.reason
    );
    let plan = build_plan(&paths, &alias, &Ok(vec![])).unwrap();
    assert_eq!(
        plan.codex
            .iter()
            .find(|c| c.thread == thread)
            .unwrap()
            .state,
        "rekey"
    );
}

#[test]
fn item1_a_backup_inside_the_root_by_its_other_spelling_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("disk/appdata/root"))).unwrap();
    let alias = Alias::new(entries(t.path()));
    // The process-wide alias is not installed; the other spelling still counts.
    let inside = t.path().join("user/appdata/root/backup");
    let e = check_backup(&paths, &alias, &inside).unwrap_err();
    assert!(e.contains("outside the data root"), "{e}");
    let parent = t.path().join("user/appdata");
    assert!(check_backup(&paths, &alias, &parent).is_err());
    check_backup(&paths, &alias, &t.path().join("elsewhere/bk")).unwrap();
}

#[test]
fn item4_codex_rollback_restores_only_the_record_it_wrote() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let thread = uuid::Uuid::new_v4().to_string();
    let s = catalog::record_launch(&paths, "c", &thread, "n", None).unwrap();
    let file = paths
        .root
        .join(layout::THREAD_LOCATIONS)
        .join(format!("{thread}.json"));
    let mut r = new_rec(&s, "run");
    r.to = Some("/new/q".into());
    session::codex_rekey_stopped(&paths, &mut r).unwrap();
    assert!(r.codex_location_after.is_some());
    // Changed by someone else since: left as is, with a warning.
    fs::write(&file, "{\"cwd\":\"/elsewhere\"}").unwrap();
    let mut changed = r.clone();
    session::rollback_stopped(&paths, &mut changed).unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "{\"cwd\":\"/elsewhere\"}"
    );
    assert!(!changed.warnings.is_empty());
    // Unchanged: the absent record is absent again.
    session::codex_rekey_stopped(&paths, &mut r).unwrap();
    r.codex_location_before = None;
    session::rollback_stopped(&paths, &mut r).unwrap();
    assert!(!file.exists());
}

#[test]
fn item3_rollback_sees_processes_on_the_new_spelling() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("user/appdata/root"))).unwrap();
    let run = Run {
        entries: entries(t.path()),
        ..Default::default()
    };
    let new_home = t.path().join("disk/appdata/root/accounts/claude/a");
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .env("CLAUDE_CONFIG_DIR", &new_home)
        .spawn()
        .unwrap();
    wait_env(child.id(), "CLAUDE_CONFIG_DIR");
    let found = new_spelling_processes(&paths, &run);
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        found
            .iter()
            .any(|p| p["pid"] == child.id() && p["variable"] == "CLAUDE_CONFIG_DIR"),
        "{found:?}"
    );
}

#[test]
fn item6_backup_verification_fails_on_a_vanished_source() {
    let t = tempfile::tempdir().unwrap();
    let (src, copy) = (t.path().join("src"), t.path().join("copy"));
    write(&src.join("a"), "a");
    write(&copy.join("a"), "a");
    write(&copy.join("b"), "b"); // its source is gone
    write(&src.join("c"), "changed");
    write(&copy.join("c"), "stale"); // recopied once, then exact
    let mut out = vec![];
    let e = verify_tree(&copy, &src, true, &mut out)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("BACKUP_VERIFY_FAILED") && e.contains("source_gone"),
        "{e}"
    );
    assert_eq!(fs::read_to_string(copy.join("c")).unwrap(), "changed");
}

#[test]
fn item7_fast_forward_never_overwrites_a_concurrent_change_and_saves_the_old_version() {
    let t = tempfile::tempdir().unwrap();
    write(&t.path().join("src"), "new");
    write(&t.path().join("dst"), "concurrent edit");
    let expected = keys::sha256_file(&t.path().join("dst")).unwrap().1;
    write(&t.path().join("dst"), "changed again");
    let versions = t.path().join("v/dst.old");
    let swapped = keys::replace_with(
        &t.path().join("src"),
        &t.path().join("dst"),
        &expected,
        &versions,
    )
    .unwrap();
    assert!(!swapped);
    assert_eq!(
        fs::read_to_string(t.path().join("dst")).unwrap(),
        "changed again"
    );
    let current = keys::sha256_file(&t.path().join("dst")).unwrap().1;
    assert!(
        keys::replace_with(
            &t.path().join("src"),
            &t.path().join("dst"),
            &current,
            &versions
        )
        .unwrap()
    );
    assert_eq!(fs::read_to_string(t.path().join("dst")).unwrap(), "new");
    assert_eq!(fs::read_to_string(&versions).unwrap(), "changed again");
}

#[test]
fn layout_version_3_admission_and_root_redirect() {
    let t = tempfile::tempdir().unwrap();
    let real = t.path().join("disk/appdata/root");
    let paths = Paths::new(Some(real.clone())).unwrap();
    let mut record = layout::read_record(&paths.root).unwrap().unwrap();
    // A version 3 record without an active alias fails closed.
    record.version = layout::ALIAS_VERSION;
    layout::write_record(&paths.root, &record).unwrap();
    let e = layout::admit(&paths.root).unwrap_err().to_string();
    assert!(e.starts_with("LAYOUT_INVALID"), "{e}");
    // Version 4 is newer than this build.
    record.version = 4;
    layout::write_record(&paths.root, &record).unwrap();
    let e = layout::admit(&paths.root).unwrap_err().to_string();
    assert!(e.starts_with("LAYOUT_UNSUPPORTED"), "{e}");
    // An active alias whose canonical root is a different, unproven directory: refused.
    let other = t.path().join("elsewhere");
    fs::create_dir_all(&other).unwrap();
    record.version = layout::ALIAS_VERSION;
    record.root_alias = Some(RootAlias {
        version: 1,
        state: "active".into(),
        root_id: "id-1".into(),
        canonical_root: other.clone(),
        previous_root: paths.root.clone(),
        entries: vec![],
        ..Default::default()
    });
    layout::write_record(&paths.root, &record).unwrap();
    let e = Paths::open(Some(real.clone())).unwrap_err().to_string();
    assert!(e.starts_with("ROOT_ALIAS_UNVERIFIED"), "{e}");
    // Same root_id and identical layout.json bytes in the canonical root: redirected.
    fs::copy(
        paths.root.join(layout::LAYOUT_FILE),
        other.join(layout::LAYOUT_FILE),
    )
    .unwrap();
    let opened = Paths::open_any_layout(Some(real.clone())).unwrap();
    assert_eq!(opened.root, other);
    assert_eq!(alias::requested_root(&opened.root), real);
}

#[test]
fn rekey_directory_and_its_exact_restore_keep_the_generation() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let conversation = uuid::Uuid::new_v4().to_string();
    let s = catalog::record_provider_launch(
        &paths,
        ProviderId::Claude,
        "a",
        &conversation,
        "n",
        Some("/old"),
        None,
    )
    .unwrap();
    let id = s.id.clone();
    let generation = catalog::find(&paths, &id).unwrap().generation();
    let (before, after) =
        catalog::rekey_directory(&paths, &id, &conversation, Some("/old"), "/new").unwrap();
    assert_ne!(after.generation(), generation);
    // Idempotent.
    assert_eq!(
        catalog::rekey_directory(&paths, &id, &conversation, Some("/old"), "/new")
            .unwrap()
            .1
            .revision,
        after.revision
    );
    assert!(
        catalog::restore_directory(
            &paths,
            &id,
            &conversation,
            "/new",
            Some("/old"),
            before,
            after.revision
        )
        .unwrap()
    );
    assert_eq!(catalog::find(&paths, &id).unwrap().generation(), generation);
}

#[test]
fn a_crash_after_any_tier_r_step_resumes_from_the_journal() {
    // Fixture without sessions: every step still runs and is journaled.
    for at in [
        "backup", "mark", "links", "registry", "claude", "codex", "activate",
    ] {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("user/appdata")).unwrap();
        let paths = Paths::new(Some(t.path().join("user/appdata/root"))).unwrap();
        let entry = Entry {
            from: t.path().join("user/appdata"),
            to: t.path().join("user/appdata"),
            proof: "same_inode".into(),
        };
        let mut run = Run {
            schema_version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            from: t.path().join("user"),
            to: t.path().join("user"),
            backup: t.path().join("bk"),
            entries: vec![entry.clone()],
            step: "admitted".into(),
            state: "running".into(),
            root_id: "r".into(),
            canonical_root: paths.root.clone(),
            previous_root: paths.root.clone(),
            created_at: now(),
            ..Default::default()
        };
        save_run(&paths, &mut run).unwrap();
        let plan = build_plan(&paths, &Alias::new(vec![entry.clone()]), &Ok(vec![])).unwrap();
        crash_at(Some(at));
        let e = drive(&paths, &mut run, &plan).unwrap_err().to_string();
        crash_at(None);
        assert!(e.contains("SIMULATED_CRASH"), "{at}: {e}");
        let mut again = latest_run(&paths).unwrap();
        assert_eq!(again.step, at);
        drive(&paths, &mut again, &plan).unwrap();
        assert_eq!(again.step, "complete", "{at}: {:?}", again.verify);
        let r = layout::read_record(&paths.root).unwrap().unwrap();
        assert_eq!(r.version, layout::ALIAS_VERSION);
        assert!(r.root_alias.unwrap().is_active());
        alias::uninstall(&[entry]);
    }
}

// ------------------------------------------------------------- review 2

fn die_during(f: &One, rec: &mut SessionRec, point: &'static str) {
    die_at(Some(point));
    let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ctx = f.ctx();
        let _ = session::rekey_stopped(&mut ctx, rec, &f.s);
    }));
    die_at(None);
    assert!(died.is_err(), "{point}: no death");
}
fn sidecar(f: &One, key: &str) -> PathBuf {
    f.projects().join(keys::key(key)).join(&f.conv)
}

// REVIEW-2 repro (adapted to the journaling `move_names`): process death after
// the rename, before phase("moved") is saved.
#[test]
fn review2_death_after_rename_before_journal_then_rollback() {
    let f = one();
    let mut rec = f.rec.clone();
    crash_at(Some("retained"));
    let _ = session::rekey_stopped(&mut f.ctx(), &mut rec, &f.s);
    crash_at(None);
    let journal = read_session(&f.paths, &f.s.id).unwrap();
    assert_eq!(journal.phase, "retained");
    // The renames that ran before the death (not journaled).
    keys::move_names(&f.projects(), &journal.retained, &keys::key(&f.new), |_| {
        Ok(())
    })
    .unwrap();
    assert!(f.new_transcript().is_file());
    let mut j = read_session(&f.paths, &f.s.id).unwrap();
    session::rollback_stopped(&f.paths, &mut j).unwrap();
    let after = read_session(&f.paths, &f.s.id).unwrap();
    eprintln!(
        "REVIEW2 state={} phase={} old_exists={} new_exists={} catalog={:?}",
        after.state,
        after.phase,
        f.old_transcript().exists(),
        f.new_transcript().exists(),
        f.dir_now()
    );
    assert!(
        f.old_transcript().is_file(),
        "per-session rollback left the transcript in the new key while the catalog names the old directory"
    );
}

// REVIEW-2 repro: death inside codex_step after save_location, before save_session.
#[test]
fn review2_codex_death_then_rerun_loses_the_original_location() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let thread = uuid::Uuid::new_v4().to_string();
    let s = catalog::record_launch(&paths, "c", &thread, "n", None).unwrap();
    let file = paths
        .root
        .join(layout::THREAD_LOCATIONS)
        .join(format!("{thread}.json"));
    crate::external::relocation::save_location(&paths, &thread, Path::new("/old/q")).unwrap();
    let original = fs::read_to_string(&file).unwrap();
    let mut r = new_rec(&s, "run");
    r.to = Some("/new/q".into());
    session::codex_rekey_stopped(&paths, &mut r).unwrap(); // death before codex_step saves `r`
    let mut again = read_session(&paths, &s.id).unwrap();
    again.to = Some("/new/q".into());
    session::codex_rekey_stopped(&paths, &mut again).unwrap(); // the rerun
    session::rollback_stopped(&paths, &mut again).unwrap();
    eprintln!(
        "REVIEW2 codex before={:?} now={:?}",
        original,
        fs::read_to_string(&file).ok()
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        original,
        "rollback did not restore the original thread location"
    );
}

/// B1 / S9: every per-session crash point, then (a) a rerun from the journal and
/// (b) a rollback from the journal, each ends consistent (files and catalog agree).
#[test]
fn every_per_session_crash_point_reruns_and_rolls_back_consistently() {
    let points = [
        "before_copy",
        "retained",
        "renamed:{conv}.jsonl", // transcript moved, sidecar not yet
        "renamed:{conv}",       // both moved, the second not yet journaled done
        "moved",
        "memory_synced",
        "catalog_switched",
    ];
    let at = |template: &str, f: &One| -> &'static str {
        Box::leak(template.replace("{conv}", &f.conv).into_boxed_str())
    };
    for template in points {
        // (a) rerun
        let f = one();
        let point = at(template, &f);
        let mut rec = f.rec.clone();
        die_during(&f, &mut rec, point);
        let mut j = read_session(&f.paths, &f.s.id).unwrap();
        session::rekey_stopped(&mut f.ctx(), &mut j, &f.s)
            .unwrap_or_else(|e| panic!("{point}: {e:#}"));
        assert_eq!(j.state, "rekeyed", "{point}");
        assert!(
            f.new_transcript().is_file() && !f.old_transcript().exists(),
            "{point}"
        );
        assert!(
            sidecar(&f, &f.new).is_dir() && !sidecar(&f, &f.old).exists(),
            "{point}"
        );
        assert_eq!(f.dir_now().as_deref(), Some(f.new.as_str()), "{point}");
        // (b) rollback
        let f = one();
        let point = at(template, &f);
        let mut rec = f.rec.clone();
        die_during(&f, &mut rec, point);
        let mut j = read_session(&f.paths, &f.s.id).unwrap();
        session::rollback_stopped(&f.paths, &mut j).unwrap_or_else(|e| panic!("{point}: {e:#}"));
        assert!(
            f.old_transcript().is_file() && !f.new_transcript().exists(),
            "{point}"
        );
        assert!(
            sidecar(&f, &f.old).is_dir() && !sidecar(&f, &f.new).exists(),
            "{point}"
        );
        assert_eq!(f.dir_now().as_deref(), Some(f.old.as_str()), "{point}");
    }
}

#[test]
fn s9_account_list_and_proc_probe_errors_block_the_move() {
    let f = one();
    // An unreadable account registry.
    let db = f.paths.root.join(layout::REGISTRY_DB);
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::write(&db, "not a database").unwrap();
    let mut rec = f.rec.clone();
    let e = session::rekey_stopped(&mut f.ctx(), &mut rec, &f.s)
        .unwrap_err()
        .to_string();
    assert!(e.contains("probe_failed"), "{e}");
    assert!(f.old_transcript().is_file() && !f.new_transcript().exists());
    // An unreadable process table.
    let e = processes_naming_in(&f.t.path().join("no-proc"), &rec.conversation_ids).unwrap_err();
    assert!(e.contains("unreadable"), "{e}");
}

#[tokio::test]
async fn s9_gate_fails_when_a_running_session_is_no_longer_recognised() {
    let f = one();
    let row = GateRow {
        session_id: f.s.id.clone(),
        name: "n".into(),
        space: None,
        provider: "claude".into(),
        account: "a".into(),
        conversation_id: f.conv.clone(),
        pane: None,
        pid: None,
        running_before: true,
        probe_error: None,
    };
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        activated_at: Some(1),
        state: "complete".into(),
        gate_before: vec![row.clone()],
        ..Default::default()
    };
    let g = gate(&f.paths, Some(&run)).await;
    assert_eq!(g["gate"], "fail", "{g}");
    // A row whose before-probe failed also fails the gate.
    let mut blind = run.clone();
    blind.gate_before[0].running_before = false;
    blind.gate_before[0].probe_error = Some("unknown".into());
    assert_eq!(gate(&f.paths, Some(&blind)).await["gate"], "fail");
    // The before-snapshot refuses Tier R when a session's state cannot be decided
    // (account `a` is not enrolled, so its Claude probe cannot run).
    let e = gate_rows(&f.paths).await.unwrap_err().to_string();
    assert!(e.contains("ROOT_MIGRATION_BLOCKED"), "{e}");
}

#[test]
fn b2_runtime_compatibility_links_reach_the_canonical_socket_and_are_safe() {
    use std::os::unix::fs::FileTypeExt;
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("disk/appdata/root"))).unwrap();
    let old_root = t.path().join("user/appdata/root");
    let mut record = layout::read_record(&paths.root).unwrap().unwrap();
    record.version = layout::ALIAS_VERSION;
    record.root_alias = Some(RootAlias {
        version: 1,
        state: "active".into(),
        root_id: "r".into(),
        canonical_root: paths.root.clone(),
        previous_root: old_root.clone(),
        entries: entries(t.path()),
        ..Default::default()
    });
    layout::write_record(&paths.root, &record).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(paths.runtime.join("control.sock")).unwrap();
    // The supervisor runs under umask 077 (main); the test process does not.
    fs::set_permissions(
        paths.runtime.join("control.sock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let r = runtime::ensure(&paths);
    let link = crate::paths::runtime_for(&old_root);
    assert_eq!(r[0]["result"], "created", "{r:?}");
    // Agent-DM's check: lstat of `<old runtime>/control.sock` is our socket.
    let m = fs::symlink_metadata(link.join("control.sock")).unwrap();
    assert!(m.file_type().is_socket() && m.mode() & 0o077 == 0);
    std::os::unix::net::UnixStream::connect(link.join("control.sock")).unwrap();
    assert_eq!(runtime::ensure(&paths)[0]["result"], "present");
    // Rollback removes only its own link.
    let run = Run {
        canonical_root: paths.root.clone(),
        previous_root: old_root.clone(),
        entries: entries(t.path()),
        ..Default::default()
    };
    runtime::remove(&run);
    assert!(fs::symlink_metadata(&link).is_err());
    drop(listener);
    // A stale private directory is moved aside (never removed); a live socket is refused.
    let stale = t.path().join("stale");
    fs::create_dir(&stale).unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o700)).unwrap();
    write(&stale.join("old.txt"), "kept");
    let r = runtime::ensure_one(&stale, &paths.runtime);
    assert!(r.starts_with("moved_aside:"), "{r}");
    let aside = PathBuf::from(r.trim_start_matches("moved_aside:"));
    assert_eq!(fs::read_to_string(aside.join("old.txt")).unwrap(), "kept");
    let live = t.path().join("live");
    fs::create_dir(&live).unwrap();
    fs::set_permissions(&live, fs::Permissions::from_mode(0o700)).unwrap();
    let _l = std::os::unix::net::UnixListener::bind(live.join("control.sock")).unwrap();
    assert!(runtime::ensure_one(&live, &paths.runtime).starts_with("refused"));
    assert!(
        live.is_dir()
            && !fs::symlink_metadata(&live)
                .unwrap()
                .file_type()
                .is_symlink()
    );
}

#[test]
fn s6_rollback_moves_back_lineage_names_created_after_the_resume_and_remaps_records() {
    let f = one();
    // No sidecar at re-key time; Claude creates one in the new key after resume.
    fs::remove_dir_all(sidecar(&f, &f.old)).unwrap();
    let mut rec = f.rec.clone();
    session::rekey_stopped(&mut f.ctx(), &mut rec, &f.s).unwrap();
    write(
        &sidecar(&f, &f.new).join("tool-results/new"),
        "after resume",
    );
    session::rollback_stopped(&f.paths, &mut rec).unwrap();
    assert!(f.old_transcript().is_file());
    assert!(
        sidecar(&f, &f.old).join("tool-results/new").is_file(),
        "sidecar followed the transcript"
    );
    assert!(!sidecar(&f, &f.new).exists());
    // Records written after activation in the new spelling map back.
    let t = f.t.path();
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        entries: entries(t),
        activated_at: Some(0),
        state: "rolling_back".into(),
        ..Default::default()
    };
    let pane = f.paths.root.join(layout::MANAGED_PANES).join("p.json");
    write(
        &pane,
        &serde_json::to_string(&json!({"home": t.join("disk/appdata/x")})).unwrap(),
    );
    assert_eq!(remap_after_activation(&f.paths, &run, &[]).unwrap(), 1);
    let v: Value = serde_json::from_slice(&fs::read(&pane).unwrap()).unwrap();
    assert_eq!(v["home"], json!(t.join("user/appdata/x")));
    // Not part of the change log, so a second rollback never undoes the remap.
    assert!(read_changes(&f.paths, &run.id).is_empty());
}

// ------------------------------------------------------------- recheck

fn gate_row(f: &One, running_before: bool) -> GateRow {
    GateRow {
        session_id: f.s.id.clone(),
        name: "n".into(),
        space: None,
        provider: "claude".into(),
        account: "a".into(),
        conversation_id: f.conv.clone(),
        pane: None,
        pid: None,
        running_before,
        probe_error: None,
    }
}

/// Recheck 1: a probe error after activation fails the gate whatever the row
/// (here: a session that was not running before; account `a` is not enrolled,
/// so its probe cannot run), and only a record of THIS run exempts a session.
#[tokio::test]
async fn recheck_gate_fails_on_any_probe_error_and_ignores_records_of_other_runs() {
    let f = one();
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        activated_at: Some(1),
        state: "complete".into(),
        gate_before: vec![gate_row(&f, false)],
        ..Default::default()
    };
    let g = gate(&f.paths, Some(&run)).await;
    assert_eq!(g["gate"], "fail", "{g}");
    assert!(!g["sessions"][0]["probe_error_now"].is_null(), "{g}");
    // A leftover record of an earlier (rolled-back) run, stuck mid Tier S.
    let mut leftover = f.rec.clone();
    leftover.run = uuid::Uuid::new_v4().to_string();
    leftover.phase = "stopped".into();
    save_session(&f.paths, &mut leftover).unwrap();
    let mut running = run.clone();
    running.gate_before = vec![gate_row(&f, true)];
    let g = gate(&f.paths, Some(&running)).await;
    assert_eq!(g["sessions"][0]["in_tier_s"], false, "{g}");
    assert_eq!(g["gate"], "fail", "{g}");
}

/// Recheck 2: a truncated or empty file at a FINAL backup name is never accepted
/// (it is re-taken through the verified writer), and a copy error fails the
/// backup leaving nothing at the final name.
#[test]
fn recheck_backup_never_accepts_a_truncated_or_empty_final_copy_and_fails_on_copy_errors() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    write(
        &paths.root.join("operations/launches/a.json"),
        "{\"phase\":\"running\",\"pad\":\"0123456789\"}",
    );
    write(
        &paths.root.join("registry/conversations/b.json"),
        "{\"owner\":\"/x\"}",
    );
    write(
        &paths.root.join("shared/claude/projects/-k/s.jsonl"),
        "line one\nline two\n",
    );
    enroll_claude(&paths, "a");
    let home = layout::target_home(&paths.root, ProviderId::Claude, "a");
    write(&home.join(".claude.json"), "{\"projects\":{\"/p\":{}}}\n");
    let bk = t.path().join("bk");
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        backup: bk.clone(),
        allow_full_copy: true,
        ..Default::default()
    };
    backup(&paths, &run).unwrap();
    let json_copy = bk.join("root/operations/launches/a.json");
    let reg_copy = bk.join("root/registry/conversations/b.json");
    let cj_copy = bk.join("claude-json/a.claude.json");
    let src = |p: &str| fs::read(paths.root.join(p)).unwrap();
    // Truncated and empty files at final names (as an interrupted direct write
    // would leave them), then a rerun: each is re-taken complete.
    fs::write(&json_copy, &src("operations/launches/a.json")[..10]).unwrap();
    fs::write(&reg_copy, b"").unwrap();
    fs::write(&cj_copy, b"").unwrap();
    backup(&paths, &run).unwrap();
    assert_eq!(
        fs::read(&json_copy).unwrap(),
        src("operations/launches/a.json")
    );
    assert_eq!(
        fs::read(&reg_copy).unwrap(),
        src("registry/conversations/b.json")
    );
    assert_eq!(
        fs::read(&cj_copy).unwrap(),
        fs::read(home.join(".claude.json")).unwrap()
    );
    let doc: Value =
        serde_json::from_slice(&fs::read(bk.join("root-backup.json")).unwrap()).unwrap();
    let listed = doc["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["file"] == "operations/launches/a.json")
        .unwrap()
        .clone();
    assert_eq!(listed["check"], "exact", "{listed}");
    // An empty copy of a non-transcript is never a "prefix".
    let mut out = vec![];
    fs::write(&reg_copy, b"").unwrap();
    verify_tree(&bk.join("root"), &paths.root, true, &mut out).unwrap();
    assert_eq!(
        fs::read(&reg_copy).unwrap(),
        src("registry/conversations/b.json")
    );
    // A copy error (reflink refused on this tmpfs without --allow-full-copy)
    // fails the backup and leaves nothing at the final name.
    fs::remove_file(&json_copy).unwrap();
    let e = verify_complete(
        &paths.root.join("operations"),
        &bk.join("root/operations"),
        Path::new("operations"),
        false,
    );
    if !crate::shared_state::migrate::reflink_supported(t.path()) {
        let e = e.unwrap_err().to_string();
        assert!(e.contains("BACKUP_VERIFY_FAILED"), "{e}");
        assert!(!json_copy.exists(), "no partial copy at the final name");
    }
}

/// Recheck 3: a before-snapshot refusal shows up in the dry run's preconditions,
/// with the same session the real run refuses on.
#[tokio::test]
async fn recheck_dry_run_reports_the_gate_snapshot_refusal() {
    let f = one();
    let t = f.t.path();
    let o = Options {
        from: t.join("user"),
        to: t.join("disk"),
        backup: Some(t.join("bk")),
        dry_run: true,
        allow_full_copy: true,
    };
    let v = migrate(&f.paths, &o).await.unwrap();
    let snap = &v["preconditions"]["gate_snapshot"];
    assert_eq!(snap["ok"], false, "{v}");
    assert!(snap["detail"].as_str().unwrap().contains(&f.s.id), "{snap}");
    let real = gate_rows(&f.paths).await.unwrap_err().to_string();
    assert!(
        real.contains("ROOT_MIGRATION_BLOCKED") && real.contains(&f.s.id),
        "{real}"
    );
    assert!(!t.join("bk").exists(), "the dry run wrote nothing");
}

/// Recheck 4: lifecycle admission skips only a fully read file in
/// root-migrations that does not parse; a read error blocks and names the file.
#[test]
fn recheck_lifecycle_skips_unparseable_but_blocks_on_unreadable_root_records() {
    let f = one();
    let d = f.paths.root.join(layout::ROOT_MIGRATIONS);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("junk.json"), "not json").unwrap();
    assert!(
        crate::lifecycle::pending(&f.paths, &f.conv, None)
            .unwrap()
            .is_none()
    );
    // A record that cannot be read (here: a directory with a .json name).
    fs::create_dir(d.join(format!("session-{}.json", f.s.id))).unwrap();
    let e = crate::lifecycle::pending(&f.paths, &f.conv, None)
        .unwrap_err()
        .to_string();
    assert!(e.contains(&format!("session-{}.json", f.s.id)), "{e}");
}

// ------------------------------------------------------------- review 3

fn enroll_claude(paths: &Paths, name: &str) {
    use crate::model::Account;
    use crate::store::Store;
    let mut store = Store::open(paths).unwrap();
    let mut state = crate::model::State::default();
    state.insert_account(Account::with_provider(
        ProviderId::Claude,
        name.into(),
        String::new(),
    ));
    store.save(&state, 1, "test", name).unwrap();
}

/// REVIEW-3 repro (B1): the F2 gate must report `fail` when the supervisor's own
/// recognition (claude::observe) no longer sees a running session. The registry
/// entry is missing (as if environ_matches had missed a spelling), so observe
/// says "stopped", but a process names the conversation on its command line (as
/// every `claude --resume <id>` does).
#[tokio::test]
async fn review3_gate_is_masked_by_the_cmdline_fallback() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    enroll_claude(&paths, "a");
    let home = layout::target_home(&paths.root, ProviderId::Claude, "a");
    let conv = uuid::Uuid::new_v4().to_string();
    let dir = t.path().join("p");
    fs::create_dir_all(&dir).unwrap();
    write(
        &home
            .join("projects")
            .join(keys::key(&dir.to_string_lossy()))
            .join(format!("{conv}.jsonl")),
        "{\"type\":\"user\"}\n",
    );
    fs::create_dir_all(home.join("sessions")).unwrap();
    let s = catalog::record_provider_launch(
        &paths,
        ProviderId::Claude,
        "a",
        &conv,
        "n",
        Some(&dir.to_string_lossy()),
        None,
    )
    .unwrap();
    let mut child = std::process::Command::new("sh")
        .args(["-c", "sleep 30", "sh", "--resume", &conv])
        .spawn()
        .unwrap();
    wait_cmdline(child.id(), &conv);
    let supervisor_view = claude::observe(&home, &conv, None).availability;
    let row = GateRow {
        session_id: s.id.clone(),
        name: "n".into(),
        space: None,
        provider: "claude".into(),
        account: "a".into(),
        conversation_id: conv.clone(),
        pane: None,
        pid: None,
        running_before: true,
        probe_error: None,
    };
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        activated_at: Some(1),
        state: "complete".into(),
        gate_before: vec![row],
        ..Default::default()
    };
    let g = gate(&paths, Some(&run)).await;
    // The plan and quiet check keep the command-line fallback (more "running").
    let plan_view = recognition(&paths, &s, &Ok(vec![]));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(supervisor_view, "stopped");
    assert_eq!(
        g["gate"], "fail",
        "the gate passes although the supervisor sees the session stopped: {g}"
    );
    assert_eq!(plan_view, Ok(true));
}

#[test]
fn s1_a_backup_interrupted_by_a_crash_is_completed_and_verified_on_rerun() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    for (rel, text) in [
        ("operations/launches/a.json", "{\"phase\":\"running\"}"),
        ("operations/launches/b.json", "{\"phase\":\"failed\"}"),
        ("messaging/claude-steer/c.json", "{}"),
        ("registry/conversations/d.json", "{\"owner\":\"/x\"}"),
        ("shared/claude/projects/-k/s.jsonl", "line\n"),
    ] {
        write(&paths.root.join(rel), text);
    }
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        backup: t.path().join("bk"),
        allow_full_copy: true,
        ..Default::default()
    };
    for point in ["backup:operations", "backup:shared"] {
        let bk = t.path().join(format!("bk-{}", point.replace(':', "-")));
        let r = Run {
            backup: bk.clone(),
            ..run.clone()
        };
        die_at(Some(point));
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| backup(&paths, &r)));
        die_at(None);
        assert!(died.is_err(), "{point}");
        // The interrupted entry is not taken for done: the rerun copies it fully.
        backup(&paths, &r).unwrap_or_else(|e| panic!("{point}: {e:#}"));
        assert!(
            bk.join("root/operations/launches/a.json").is_file(),
            "{point}"
        );
        assert!(
            bk.join("root/operations/launches/b.json").is_file(),
            "{point}"
        );
        assert!(
            bk.join("shared-claude-projects/-k/s.jsonl").is_file(),
            "{point}"
        );
        let doc: Value =
            serde_json::from_slice(&fs::read(bk.join("root-backup.json")).unwrap()).unwrap();
        assert!(doc["file_count"].as_u64().unwrap() >= 5, "{point}: {doc}");
    }
    // A copy that lost a file is completed by the source-side verification.
    let bk = t.path().join("bk-ok");
    let r = Run {
        backup: bk.clone(),
        ..run.clone()
    };
    backup(&paths, &r).unwrap();
    fs::remove_file(bk.join("root/operations/launches/b.json")).unwrap();
    verify_complete(
        &paths.root.join("operations"),
        &bk.join("root/operations"),
        Path::new("operations"),
        true,
    )
    .unwrap();
    assert!(bk.join("root/operations/launches/b.json").is_file());
}

#[test]
fn s3_move_back_refuses_when_both_names_exist_and_the_old_one_is_not_ours() {
    let f = one();
    let mut rec = f.rec.clone();
    session::rekey_stopped(&mut f.ctx(), &mut rec, &f.s).unwrap();
    // Something re-created the transcript in the old key after the rename.
    write(&f.old_transcript(), "{\"type\":\"someone else\"}\n");
    let e = session::rollback_stopped(&f.paths, &mut rec)
        .unwrap_err()
        .to_string();
    assert!(e.contains("ROOT_REKEY_REVERT_CONFLICT"), "{e}");
    let saved = read_session(&f.paths, &f.s.id).unwrap();
    assert_eq!(saved.state, "uncertain");
    assert!(f.new_transcript().is_file(), "the moved transcript stays");
    assert_eq!(
        fs::read_to_string(f.old_transcript()).unwrap(),
        "{\"type\":\"someone else\"}\n"
    );
}

#[test]
fn s5_an_old_spelling_app_server_thread_matches_its_pane_only_through_the_alias() {
    let t = tempfile::tempdir().unwrap();
    let new_home = t.path().join("disk/appdata/root/accounts/codex/c");
    let old_home = t.path().join("user/appdata/root/accounts/codex/c");
    let new_cwd = t.path().join("disk/live/q");
    let old_cwd = t.path().join("user/live/q");
    for d in [&new_home, &old_home, &new_cwd, &old_cwd] {
        fs::create_dir_all(d).unwrap();
    }
    let thread = uuid::Uuid::new_v4().to_string();
    let rollout_old = old_home.join(format!("sessions/2026/rollout-{thread}.jsonl"));
    // The pane: its process cwd and home as the supervisor sees them (new spelling).
    let pane: crate::external::Instance = serde_json::from_value(json!({
        "pane":"%1","tmux_session":"s","pid":1,"cwd":new_cwd,"thread_id":"","rollout":"",
        "home":new_home,"binary":"codex","resume_options":[]}))
    .unwrap();
    // The long-lived app-server reports the old spelling for path and cwd.
    let threads = vec![json!({"id":thread,"cwd":old_cwd,"path":rollout_old,"parentThreadId":null})];
    let panes = vec![pane];
    assert!(
        crate::external::match_app_server_thread(&panes, 0, &new_home, &threads).is_none(),
        "without the alias the two spellings differ"
    );
    let e = entries(t.path());
    alias::install(&e);
    let matched = crate::external::match_app_server_thread(&panes, 0, &new_home, &threads)
        .map(|(_, id, path)| (id, path));
    // By exact id as well.
    let mut known = panes.clone();
    known[0].thread_id = thread.clone();
    let exact = crate::external::match_app_server_thread(&known, 0, &new_home, &threads)
        .map(|(_, id, _)| id);
    alias::uninstall(&e);
    assert_eq!(matched, Some((thread.clone(), rollout_old)));
    assert_eq!(exact, Some(thread));
}

/// Review 3 S6: Tier R with real data (links, registry records, one stopped
/// Claude session, one Codex thread) and a non-identity alias; a death before
/// and after each step's journal save, then a rerun or a rollback, each ends
/// consistent.
#[test]
fn s6_tier_r_crash_before_and_after_each_step_reruns_and_rolls_back() {
    struct R {
        t: tempfile::TempDir,
        paths: Paths,
        conv: String,
        claude_id: String,
        thread: String,
        alias: Alias,
    }
    impl R {
        fn old_dir(&self) -> PathBuf {
            self.t.path().join("user/live/p")
        }
        fn new_dir(&self) -> PathBuf {
            self.t.path().join("disk/live/p")
        }
        fn store(&self) -> PathBuf {
            crate::shared_state::target(&self.paths.root, "projects")
        }
        fn transcript(&self, dir: &Path) -> PathBuf {
            self.store()
                .join(keys::key(&dir.to_string_lossy()))
                .join(format!("{}.jsonl", self.conv))
        }
        fn link(&self) -> PathBuf {
            layout::target_home(&self.paths.root, ProviderId::Claude, "a").join("projects")
        }
        fn pane(&self) -> Value {
            serde_json::from_slice(
                &fs::read(self.paths.root.join(layout::MANAGED_PANES).join("p.json")).unwrap(),
            )
            .unwrap()
        }
        fn location(&self) -> Option<String> {
            crate::external::relocation::saved_location(&self.paths, &self.thread)
        }
        fn dir_now(&self) -> Option<String> {
            catalog::find(&self.paths, &self.claude_id)
                .unwrap()
                .directory
        }
    }
    fn fixture() -> R {
        let t = tempfile::tempdir().unwrap();
        let tp = t.path();
        fs::create_dir_all(tp.join("disk/appdata")).unwrap();
        fs::create_dir_all(tp.join("disk/live/p")).unwrap();
        fs::create_dir_all(tp.join("disk/live/q")).unwrap();
        fs::create_dir_all(tp.join("user")).unwrap();
        std::os::unix::fs::symlink(tp.join("disk/appdata"), tp.join("user/appdata")).unwrap();
        std::os::unix::fs::symlink(tp.join("disk/live"), tp.join("user/live")).unwrap();
        let paths = Paths::new(Some(tp.join("disk/appdata/root"))).unwrap();
        enroll_claude(&paths, "a");
        crate::shared_state::enable(&paths.root).unwrap();
        let home = layout::target_home(&paths.root, ProviderId::Claude, "a");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let store = crate::shared_state::target(&paths.root, "projects");
        // The account links name the old spelling of the store (as on the live root).
        for name in crate::shared_state::NAMES {
            crate::paths::private_dir(&crate::shared_state::target(&paths.root, name)).unwrap();
            std::os::unix::fs::symlink(
                tp.join("user/appdata/root/shared/claude").join(name),
                home.join(name),
            )
            .unwrap();
        }
        let conv = uuid::Uuid::new_v4().to_string();
        let old = tp.join("user/live/p");
        write(
            &store
                .join(keys::key(&old.to_string_lossy()))
                .join(format!("{conv}.jsonl")),
            "{\"type\":\"user\"}\n",
        );
        let s = catalog::record_provider_launch(
            &paths,
            ProviderId::Claude,
            "a",
            &conv,
            "n",
            Some(&old.to_string_lossy()),
            None,
        )
        .unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        catalog::record_launch(&paths, "c", &thread, "q", None).unwrap();
        crate::external::relocation::save_location(&paths, &thread, &tp.join("user/live/q"))
            .unwrap();
        write(
            &paths.root.join(layout::MANAGED_PANES).join("p.json"),
            &serde_json::to_string(&json!({"home": tp.join("user/appdata/root/accounts/codex/c")}))
                .unwrap(),
        );
        let alias = Alias::new(entries(tp));
        R {
            claude_id: s.id,
            t,
            paths,
            conv,
            thread,
            alias,
        }
    }
    fn start(r: &R) -> Run {
        let layout_bytes = fs::read(r.paths.root.join(layout::LAYOUT_FILE)).unwrap();
        let mut run = Run {
            schema_version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            from: r.t.path().join("user"),
            to: r.t.path().join("disk"),
            backup: r.t.path().join("bk"),
            allow_full_copy: true,
            entries: r.alias.entries.clone(),
            step: "admitted".into(),
            state: "running".into(),
            root_id: "r".into(),
            canonical_root: r.paths.root.clone(),
            previous_root: r.paths.root.clone(),
            layout_before: serde_json::to_value(
                layout::read_record(&r.paths.root).unwrap().unwrap(),
            )
            .unwrap(),
            layout_before_sha256: sha(&layout_bytes),
            created_at: now(),
            ..Default::default()
        };
        save_run(&r.paths, &mut run).unwrap();
        run
    }
    fn plan(r: &R) -> Plan {
        build_plan(&r.paths, &r.alias, &Ok(vec![])).unwrap()
    }
    let steps = [
        "backup", "mark", "links", "registry", "claude", "codex", "activate", "verify",
    ];
    let mut points = vec![];
    for s in steps {
        points.push(format!("before:{s}"));
        points.push(s.to_string());
    }
    for point in points {
        let point: &'static str = Box::leak(point.into_boxed_str());
        for rollback_after in [false, true] {
            let r = fixture();
            let mut run = start(&r);
            let p = plan(&r);
            die_at(Some(point));
            let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = drive(&r.paths, &mut run, &p);
            }));
            die_at(None);
            assert!(died.is_err(), "{point}: no death");
            if rollback_after {
                let out = rollback(&r.paths, false).unwrap_or_else(|e| panic!("{point}: {e:#}"));
                assert_eq!(out["rolled_back"], true, "{point}: {out}");
                assert!(r.transcript(&r.old_dir()).is_file(), "{point}");
                assert!(!r.transcript(&r.new_dir()).exists(), "{point}");
                assert_eq!(
                    r.dir_now().as_deref(),
                    Some(&*r.old_dir().to_string_lossy()),
                    "{point}"
                );
                assert_eq!(
                    r.location().as_deref(),
                    Some(&*r.t.path().join("user/live/q").to_string_lossy()),
                    "{point}"
                );
                assert_eq!(
                    r.pane()["home"],
                    json!(r.t.path().join("user/appdata/root/accounts/codex/c")),
                    "{point}"
                );
                assert_eq!(
                    fs::read_link(r.link()).unwrap(),
                    r.t.path().join("user/appdata/root/shared/claude/projects"),
                    "{point}"
                );
                assert_eq!(
                    layout::read_record(&r.paths.root).unwrap().unwrap().version,
                    layout::VERSION,
                    "{point}"
                );
            } else {
                let mut again = latest_run(&r.paths).unwrap();
                if step_index(&again.step) >= step_index("mark") {
                    alias::install(&again.entries);
                }
                let p = plan(&r);
                drive(&r.paths, &mut again, &p).unwrap_or_else(|e| panic!("{point}: {e:#}"));
                assert_eq!(again.step, "complete", "{point}: {:?}", again.verify);
                assert!(r.transcript(&r.new_dir()).is_file(), "{point}");
                assert!(!r.transcript(&r.old_dir()).exists(), "{point}");
                assert_eq!(
                    r.dir_now().as_deref(),
                    Some(&*r.new_dir().to_string_lossy()),
                    "{point}"
                );
                assert_eq!(
                    r.location().as_deref(),
                    Some(&*r.t.path().join("disk/live/q").to_string_lossy()),
                    "{point}"
                );
                assert_eq!(
                    r.pane()["home"],
                    json!(r.t.path().join("disk/appdata/root/accounts/codex/c")),
                    "{point}"
                );
                assert_eq!(
                    fs::read_link(r.link()).unwrap(),
                    r.paths.root.join("shared/claude/projects"),
                    "{point}"
                );
            }
            alias::uninstall(&r.alias.entries);
        }
    }
}
