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
    let dry = keys::sync_memory(&src, &dst, &mut base, Some(&backup), "k", true).unwrap();
    assert_eq!(dry.copied, ["MEMORY.md", "only-src.md"]);
    assert!(!dst.join("MEMORY.md").exists() && base.is_empty() && !backup.exists());
    // Missing -> copied; identical -> nothing; only in target -> left alone.
    let r = keys::sync_memory(&src, &dst, &mut base, Some(&backup), "k", false).unwrap();
    assert_eq!(r.copied, ["MEMORY.md", "only-src.md"]);
    assert_eq!(r.identical, ["same.md"]);
    assert_eq!(r.only_in_target, ["only-dst.md"]);
    assert_eq!(
        fs::read_to_string(dst.join("MEMORY.md")).unwrap(),
        "index v1"
    );
    // Newer in src (dst still equals the base): fast-forward, the base version saved.
    write(&src.join("MEMORY.md"), "index v2");
    let r = keys::sync_memory(&src, &dst, &mut base, Some(&backup), "k", false).unwrap();
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
    let r = keys::sync_memory(&src, &dst, &mut base, Some(&backup), "k", false).unwrap();
    assert_eq!(r.kept_newer, ["only-src.md"]);
    assert_eq!(
        fs::read_to_string(dst.join("only-src.md")).unwrap(),
        "edited on the new side"
    );
    // Both changed: conflict; both files unchanged in place, both copied aside.
    write(&src.join("MEMORY.md"), "old side edit");
    write(&dst.join("MEMORY.md"), "new side edit");
    let r = keys::sync_memory(&src, &dst, &mut base, Some(&backup), "k", false).unwrap();
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
        keys::sync_memory(&src, &dst, &mut base, Some(&backup), "k", false)
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
    let r = keys::sync_memory(&src, &dst, &mut base, None, "k", false).unwrap();
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
    let moved = keys::move_names(&projects, &retained, "-new").unwrap();
    assert_eq!(moved.len(), 2);
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
    let e = keys::move_names(&projects, &retained, "-new")
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
    // Another branch holding the share makes it unprovable.
    fs::create_dir_all(mnt.join("disk2/appdata")).unwrap();
    assert!(
        prove(&mnt.join("user/appdata"), &mnt.join("disk1/appdata"), false)
            .unwrap_err()
            .starts_with("other_branch:")
    );
    // user0 (the array view) is not another branch.
    fs::remove_dir(mnt.join("disk2/appdata")).unwrap();
    fs::create_dir_all(mnt.join("user0/appdata")).unwrap();
    assert_eq!(
        prove(&mnt.join("user/appdata"), &mnt.join("disk1/appdata"), true).unwrap_err(),
        "probe_required"
    );
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
        let plan = build_plan(&paths, &Alias::new(vec![entry.clone()])).unwrap();
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
