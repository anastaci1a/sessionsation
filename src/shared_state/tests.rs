use super::migrate::{self, Options};
use super::*;
use crate::{
    model::{Account, State},
    store::Store,
};
use std::{cell::RefCell, os::unix::fs::PermissionsExt};

thread_local! {
    static CRASH_AT: RefCell<Option<&'static str>> = const { RefCell::new(None) };
}
/// Simulated crash: the journal keeps the phase just written, nothing after runs.
pub(super) fn crash_check(name: &str) -> Result<()> {
    ensure!(
        CRASH_AT.with(|c| *c.borrow()) != Some(name),
        "SIMULATED_CRASH at {name}"
    );
    Ok(())
}
fn crash_at(phase: Option<&'static str>) {
    CRASH_AT.with(|c| *c.borrow_mut() = phase);
}

pub(super) struct Fx {
    pub _dir: tempfile::TempDir,
    pub paths: Paths,
    store: Store,
    state: State,
}
pub(super) fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let store = Store::open(&paths).unwrap();
    Fx {
        _dir: dir,
        paths,
        store,
        state: State::default(),
    }
}
impl Fx {
    /// Enroll a managed Claude account and create its (private) home.
    pub fn account(&mut self, name: &str) -> PathBuf {
        let (home, _) = crate::provider::claude::create_home(&self.paths, name).unwrap();
        let a = Account::with_provider(ProviderId::Claude, name.into(), String::new());
        self.state.insert_account(a);
        self.store.save(&self.state, 1, "test", name).unwrap();
        home
    }
    fn adopted(&mut self, name: &str, dir: &Path) {
        let mut a = Account::with_provider(ProviderId::Claude, name.into(), String::new());
        a.config_home = Some(dir.to_path_buf());
        self.state.insert_account(a);
        self.store.save(&self.state, 1, "test", name).unwrap();
    }
    fn root(&self) -> &Path {
        &self.paths.root
    }
    fn migrate(&self, name: &str) -> Result<Value> {
        migrate::migrate(
            &self.paths,
            &Options {
                account: name.into(),
                allow_full_copy: true,
                ..Default::default()
            },
        )
    }
}

const CONVERSATION_A: &str = "11111111-1111-4111-8111-111111111111";
const CONVERSATION_B: &str = "22222222-2222-4222-8222-222222222222";
const SLUG: &str = "-work-project";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
/// Plausible Claude state of one conversation under a config dir.
fn seed(home: &Path, id: &str, tag: &str) {
    write(
        &home.join(format!("projects/{SLUG}/{id}.jsonl")),
        &format!("transcript {tag}\n"),
    );
    write(
        &home.join(format!("projects/{SLUG}/{id}/tool-results/r.txt")),
        &format!("result {tag}"),
    );
    write(
        &home.join(format!("file-history/{id}/h1")),
        &format!("history {tag}"),
    );
    write(&home.join(format!("session-env/{id}/e1")), tag);
    write(&home.join(format!("todos/{id}-agent-1.json")), "[]");
}
fn ino(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().ino()
}
fn exchange_supported() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let (x, y) = (dir.path().join("x"), dir.path().join("y"));
    fs::create_dir(&x).unwrap();
    symlink("t", &y).unwrap();
    rename_exchange(&x, &y).is_ok()
}
macro_rules! require_exchange {
    () => {
        if !exchange_supported() {
            eprintln!(
                "SKIPPED {}: renameat2(RENAME_EXCHANGE) is not supported on this test file system",
                module_path!()
            );
            return;
        }
    };
}
fn assert_all_trusted(home: &Path) {
    for n in NAMES {
        assert!(trusted_link(&home.join(n)), "{n} is not a trusted link");
    }
}

// ----------------------------------------------------------- the trust helper

#[test]
fn only_the_exact_account_home_link_is_trusted() {
    let mut f = fx();
    let home = f.account("a");
    enable(f.root()).unwrap();
    assert_eq!(
        ensure_links(&f.paths, &home).unwrap(),
        Ensure::Linked {
            created: NAMES.to_vec()
        }
    );
    assert_all_trusted(&home);
    assert!(fully_linked(&home));
    let link = home.join("projects");
    let good = target(f.root(), "projects");

    // Owned by another user.
    assert!(trusted_link(&link));
    assert!(!trusted_link_as(&link, unsafe { libc::geteuid() } + 1));
    // A relative spelling of the same directory is not "exactly" the target.
    fs::remove_file(&link).unwrap();
    symlink("../../../shared/claude/projects", &link).unwrap();
    assert_eq!(fs::canonicalize(&link).unwrap(), good);
    assert!(!trusted_link(&link), "relative target");
    // Pointing elsewhere (even to a private directory of ours).
    let elsewhere = f.root().join("elsewhere");
    crate::paths::private_dir(&elsewhere).unwrap();
    fs::remove_file(&link).unwrap();
    symlink(&elsewhere, &link).unwrap();
    assert!(!trusted_link(&link), "link to another directory");
    // Replaced by a link to another shared name.
    fs::remove_file(&link).unwrap();
    symlink(target(f.root(), "todos"), &link).unwrap();
    assert!(!trusted_link(&link), "link to the wrong shared name");
    // Replaced by a link to a link that leads to the right place.
    let hop = f.root().join("hop");
    symlink(&good, &hop).unwrap();
    fs::remove_file(&link).unwrap();
    symlink(&hop, &link).unwrap();
    assert!(!trusted_link(&link), "link via another link");
    // The right link again.
    fs::remove_file(&link).unwrap();
    symlink(&good, &link).unwrap();
    assert!(trusted_link(&link));

    // A listed name outside an account home, an unlisted name, a non-home parent.
    let stray = f.root().join("stray");
    crate::paths::private_dir(&stray).unwrap();
    symlink(&good, stray.join("projects")).unwrap();
    assert!(!trusted_link(&stray.join("projects")));
    symlink(&good, home.join("settings")).unwrap();
    assert!(!trusted_link(&home.join("settings")));
    crate::paths::private_dir(&home.join("nested")).unwrap();
    symlink(&good, home.join("nested/projects")).unwrap();
    assert!(!trusted_link(&home.join("nested/projects")));
    let adopted = f._dir.path().join("adopted");
    crate::paths::private_dir(&adopted).unwrap();
    symlink(&good, adopted.join("projects")).unwrap();
    assert!(!trusted_link(&adopted.join("projects")), "adopted dir");

    // The target must be a real private directory, not a link or group-open.
    fs::set_permissions(&good, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(!trusted_link(&link), "group-readable target");
    fs::set_permissions(&good, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(trusted_link(&link));
    let moved = f.root().join("moved-away");
    fs::rename(&good, &moved).unwrap();
    symlink(&moved, &good).unwrap();
    assert!(!trusted_link(&link), "target replaced by a symlink");
    fs::remove_file(&good).unwrap();
    fs::rename(&moved, &good).unwrap();
    assert!(trusted_link(&link));
    // A real directory or file at the name is never a link.
    assert!(!trusted_link(&home.join("nested")));
}

#[test]
fn a_link_owned_by_another_user_is_not_trusted_when_running_as_root() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("SKIPPED: chown to another user needs root");
        return;
    }
    let mut f = fx();
    let home = f.account("a");
    enable(f.root()).unwrap();
    ensure_links(&f.paths, &home).unwrap();
    let link = home.join("projects");
    assert!(trusted_link(&link));
    std::os::unix::fs::lchown(&link, Some(65534), None).unwrap();
    assert!(!trusted_link(&link));
}

#[test]
fn nothing_happens_until_the_feature_is_enabled_and_adopted_homes_are_never_linked() {
    let mut f = fx();
    let home = f.account("a");
    assert_eq!(ensure_links(&f.paths, &home).unwrap(), Ensure::Disabled);
    assert!(fs::symlink_metadata(f.root().join("shared")).is_err());
    assert!(fs::symlink_metadata(home.join("projects")).is_err());
    enable(f.root()).unwrap();
    let outside = f._dir.path().join("config");
    crate::paths::private_dir(&outside).unwrap();
    assert_eq!(
        ensure_links(&f.paths, &outside).unwrap(),
        Ensure::NotApplicable
    );
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    f.adopted("adopt", &outside);
    let error = f.migrate("adopt").unwrap_err().to_string();
    assert!(error.starts_with("SHARED_STATE_NOT_APPLICABLE"), "{error}");
}

#[test]
fn ensure_links_creates_repairs_and_refuses_data_and_foreign_links() {
    let mut f = fx();
    let home = f.account("a");
    enable(f.root()).unwrap();
    // An empty real directory is replaced; the others are created.
    fs::create_dir(home.join("todos")).unwrap();
    let Ensure::Linked { created } = ensure_links(&f.paths, &home).unwrap() else {
        panic!()
    };
    assert_eq!(created, NAMES.to_vec());
    assert_all_trusted(&home);
    assert_eq!(
        ensure_links(&f.paths, &home).unwrap(),
        Ensure::Linked { created: vec![] },
        "idempotent"
    );
    // A deleted link is repaired.
    fs::remove_file(home.join("projects")).unwrap();
    assert_eq!(
        ensure_links(&f.paths, &home).unwrap(),
        Ensure::Linked {
            created: vec!["projects"]
        }
    );

    // A real directory with data is never merged here.
    let b = f.account("b");
    seed(&b, CONVERSATION_A, "b");
    let error = ensure_links(&f.paths, &b).unwrap_err().to_string();
    assert!(error.starts_with("SHARED_STATE_UNMIGRATED"), "{error}");
    assert!(
        error.contains("shared migrate --account claude:b"),
        "{error}"
    );
    assert!(
        fs::symlink_metadata(b.join("projects")).unwrap().is_dir(),
        "data directory untouched"
    );
    assert!(
        fs::read_dir(target(f.root(), "projects"))
            .unwrap()
            .next()
            .is_none(),
        "nothing merged into the store"
    );

    // Another link, or a replaced link, is refused and left as found.
    let c = f.account("c");
    let elsewhere = f.root().join("elsewhere");
    crate::paths::private_dir(&elsewhere).unwrap();
    symlink(&elsewhere, c.join("projects")).unwrap();
    let error = ensure_links(&f.paths, &c).unwrap_err().to_string();
    assert!(error.starts_with("SHARED_STATE_LINK_UNTRUSTED"), "{error}");
    assert_eq!(fs::read_link(c.join("projects")).unwrap(), elsewhere);
    assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
}

#[test]
fn generic_symlink_checks_still_refuse_the_links() {
    use crate::external::archiving::native as files;
    let mut f = fx();
    let home = f.account("a");
    enable(f.root()).unwrap();
    ensure_links(&f.paths, &home).unwrap();
    write(&target(f.root(), "projects").join("keep/x.jsonl"), "data");
    let link = home.join("projects");
    // Copying, digesting and removing never walk through a link.
    for result in [
        files::reject_symlink(&link).map(|_| ()),
        files::tree_digest(&link).map(|_| ()),
        files::remove_tree(&link),
        files::copy_tree(&link, &f._dir.path().join("copy")),
    ] {
        assert!(
            format!("{:#}", result.unwrap_err()).contains("HISTORY_SYMLINK"),
            "a link must be refused by the generic walkers"
        );
    }
    assert!(target(f.root(), "projects").join("keep/x.jsonl").exists());
    // The only accepted symlink parent is a trusted link.
    files::ensure_parent(&home.join("file-history/some-id")).unwrap();
    let elsewhere = f.root().join("elsewhere");
    crate::paths::private_dir(&elsewhere).unwrap();
    fs::remove_file(home.join("todos")).unwrap();
    symlink(&elsewhere, home.join("todos")).unwrap();
    let error = files::ensure_parent(&home.join("todos/x.json")).unwrap_err();
    assert!(format!("{error:#}").contains("HISTORY_SYMLINK"));
}

#[test]
fn detaching_removes_only_the_links_and_deleting_the_home_keeps_shared_data() {
    let mut f = fx();
    let home = f.account("a");
    enable(f.root()).unwrap();
    ensure_links(&f.paths, &home).unwrap();
    seed(&home, CONVERSATION_A, "a");
    write(&home.join("settings.json"), "{}");
    let foreign = f.root().join("foreign");
    write(&foreign.join("precious"), "x");
    fs::remove_file(home.join("todos")).unwrap();
    symlink(&foreign, home.join("todos")).unwrap();
    let removed = detach_links(&home).unwrap();
    assert_eq!(removed, vec!["projects", "file-history", "session-env"]);
    assert!(
        fs::symlink_metadata(home.join("todos"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "an untrusted link is not ours to remove"
    );
    fs::remove_dir_all(&home).unwrap();
    for n in ["projects", "file-history", "session-env"] {
        assert!(target(f.root(), n).is_dir());
    }
    assert!(
        target(f.root(), "projects")
            .join(format!("{SLUG}/{CONVERSATION_A}.jsonl"))
            .exists()
    );
    assert!(foreign.join("precious").exists());
}

#[test]
fn a_renamed_home_keeps_trusted_links_and_data() {
    let mut f = fx();
    let home = f.account("a");
    enable(f.root()).unwrap();
    ensure_links(&f.paths, &home).unwrap();
    seed(&home, CONVERSATION_A, "a");
    let new = crate::layout::target_home(f.root(), ProviderId::Claude, "renamed");
    fs::rename(&home, &new).unwrap();
    assert_all_trusted(&new);
    assert!(
        new.join(format!("projects/{SLUG}/{CONVERSATION_A}.jsonl"))
            .exists()
    );
}

// ------------------------------------------------------------- the migration

#[test]
fn first_account_is_adopted_by_one_exchange_without_copying_or_a_missing_moment() {
    require_exchange!();
    let mut f = fx();
    let home = f.account("a");
    seed(&home, CONVERSATION_A, "a");
    write(
        &home.join(format!("projects/{SLUG}/memory/MEMORY.md")),
        "mem-a",
    );
    write(&home.join("settings.json"), "{\"keep\":true}");
    let dir_inodes: Vec<_> = NAMES.iter().map(|n| ino(&home.join(n))).collect();
    let transcript = home.join(format!("projects/{SLUG}/{CONVERSATION_A}.jsonl"));
    let inode = ino(&transcript);
    // A "live Claude process": an open append handle on the transcript.
    let mut live = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();

    // Observer: the account path must never be missing during the migration.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer = {
        let (stop, paths): (_, Vec<PathBuf>) =
            (stop.clone(), NAMES.iter().map(|n| home.join(n)).collect());
        std::thread::spawn(move || {
            let mut missing = 0u64;
            let mut seen = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for p in &paths {
                    match fs::symlink_metadata(p) {
                        Ok(_) => seen += 1,
                        Err(_) => missing += 1,
                    }
                }
            }
            (missing, seen)
        })
    };
    let out = f.migrate("a").unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (missing, seen) = observer.join().unwrap();
    assert!(seen > 0);
    assert_eq!(
        missing, 0,
        "the account path was missing during the exchange"
    );

    assert_eq!(out["complete"], true, "{out}");
    assert_all_trusted(&home);
    for (n, before) in NAMES.iter().zip(dir_inodes) {
        assert_eq!(out["results"][n]["link"], "adopted", "{out}");
        assert_eq!(
            ino(&target(f.root(), n)),
            before,
            "{n}: the same directory inode now lives in the store (nothing was copied)"
        );
    }
    assert_eq!(
        fs::metadata(target(f.root(), "projects")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(ino(&transcript), inode, "same file through the link");
    use std::io::Write;
    live.write_all(b"appended by the live process\n").unwrap();
    assert!(
        fs::read_to_string(
            target(f.root(), "projects").join(format!("{SLUG}/{CONVERSATION_A}.jsonl"))
        )
        .unwrap()
        .ends_with("appended by the live process\n"),
        "the open file descriptor still writes to the same inode"
    );
    assert_eq!(
        fs::read_to_string(home.join("settings.json")).unwrap(),
        "{\"keep\":true}"
    );
    // Backups hold the pre-migration content; the journal is complete.
    let backup = PathBuf::from(out["backup_dir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(backup.join(format!("projects/{SLUG}/memory/MEMORY.md"))).unwrap(),
        "mem-a"
    );
    assert!(
        backup
            .join(format!("todos/{CONVERSATION_A}-agent-1.json"))
            .exists()
    );
    assert!(backup.starts_with(f.root().join("backups")));
    let journals: Vec<_> = fs::read_dir(f.root().join(crate::layout::SHARED_MIGRATIONS))
        .unwrap()
        .collect();
    assert_eq!(journals.len(), 1);
    // Idempotent: nothing more to do, no second journal or backup.
    let again = f.migrate("a").unwrap();
    assert_eq!(again["already_migrated"], true);
    assert_eq!(
        fs::read_dir(f.root().join(crate::layout::SHARED_MIGRATIONS))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(fs::read_dir(f.root().join("backups")).unwrap().count(), 1);
}

#[test]
fn second_account_merges_without_overwriting_and_resumes_after_conflicts() {
    require_exchange!();
    let mut f = fx();
    let a = f.account("a");
    seed(&a, CONVERSATION_A, "a");
    write(
        &a.join(format!("projects/{SLUG}/memory/MEMORY.md")),
        "mem-a",
    );
    f.migrate("a").unwrap();
    let b = f.account("b");
    seed(&b, CONVERSATION_B, "b");
    write(
        &b.join(format!("projects/{SLUG}/memory/MEMORY.md")),
        "mem-b",
    );
    write(&b.join("projects/-other/x.jsonl"), "only b");
    let b_projects_before = ino(&b.join("projects"));

    let dry = migrate::migrate(
        &f.paths,
        &Options {
            account: "b".into(),
            dry_run: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(dry["names"]["projects"]["action"], "conflicts");
    assert_eq!(
        dry["names"]["projects"]["conflicts"][0],
        format!("{SLUG}/memory/MEMORY.md")
    );
    assert_eq!(dry["names"]["file-history"]["action"], "merge_then_link");
    assert_eq!(dry["would_complete"], false);
    assert_eq!(
        fs::read_dir(f.root().join(crate::layout::SHARED_MIGRATIONS))
            .unwrap()
            .count(),
        1,
        "a dry run journals nothing"
    );
    assert_eq!(ino(&b.join("projects")), b_projects_before);

    let out = f.migrate("b").unwrap();
    assert_eq!(out["complete"], false, "{out}");
    assert_eq!(
        out["results"]["projects"]["conflicts"],
        json!([format!("{SLUG}/memory/MEMORY.md")])
    );
    // The conflicting file was neither overwritten nor lost.
    assert_eq!(
        fs::read_to_string(target(f.root(), "projects").join(format!("{SLUG}/memory/MEMORY.md")))
            .unwrap(),
        "mem-a"
    );
    assert_eq!(
        fs::read_to_string(b.join(format!("projects/{SLUG}/memory/MEMORY.md"))).unwrap(),
        "mem-b"
    );
    // Names without conflicts are linked; the conflicted one stays a real directory.
    assert!(trusted_link(&b.join("file-history")));
    assert!(trusted_link(&b.join("todos")));
    assert!(trusted_link(&b.join("session-env")));
    assert!(fs::symlink_metadata(b.join("projects")).unwrap().is_dir());
    // Non-conflicting entries were moved into the store.
    let store_projects = target(f.root(), "projects");
    assert!(store_projects.join("-other/x.jsonl").exists());
    assert!(
        store_projects
            .join(format!("{SLUG}/{CONVERSATION_B}.jsonl"))
            .exists()
    );
    assert!(
        store_projects
            .join(format!("{SLUG}/{CONVERSATION_A}.jsonl"))
            .exists()
    );
    // ensure_links keeps refusing the half-migrated name.
    assert!(
        ensure_links(&f.paths, &b)
            .unwrap_err()
            .to_string()
            .starts_with("SHARED_STATE_UNMIGRATED")
    );

    // The operator resolves the conflict (here: keeps the store's copy) and repeats.
    fs::remove_file(b.join(format!("projects/{SLUG}/memory/MEMORY.md"))).unwrap();
    let done = f.migrate("b").unwrap();
    assert_eq!(done["complete"], true, "{done}");
    assert_all_trusted(&b);
    assert_eq!(
        fs::read_dir(f.root().join(crate::layout::SHARED_MIGRATIONS))
            .unwrap()
            .count(),
        2,
        "the unfinished journal was resumed, not duplicated"
    );
    // Both accounts now see the same files.
    for home in [&a, &b] {
        for id in [CONVERSATION_A, CONVERSATION_B] {
            assert!(home.join(format!("projects/{SLUG}/{id}.jsonl")).exists());
            assert!(home.join(format!("file-history/{id}/h1")).exists());
        }
    }
    // The backup of b kept the conflicting file.
    let backup = PathBuf::from(out["backup_dir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(backup.join(format!("projects/{SLUG}/memory/MEMORY.md"))).unwrap(),
        "mem-b"
    );
}

#[test]
fn merge_without_conflicts_links_atomically() {
    require_exchange!();
    let mut f = fx();
    let a = f.account("a");
    seed(&a, CONVERSATION_A, "a");
    f.migrate("a").unwrap();
    let b = f.account("b");
    seed(&b, CONVERSATION_B, "b");
    let out = f.migrate("b").unwrap();
    assert_eq!(out["complete"], true, "{out}");
    for n in NAMES {
        assert_eq!(out["results"][n]["link"], "merged");
    }
    assert!(fs::read_dir(&b).unwrap().all(|e| {
        let name = e.unwrap().file_name();
        !name.to_string_lossy().contains("shared-link")
    }));
}

#[test]
fn an_interrupted_migration_resumes_from_every_phase() {
    require_exchange!();
    for phase in ["backing_up", "backed_up", "linking"] {
        let mut f = fx();
        let home = f.account("a");
        seed(&home, CONVERSATION_A, "a");
        crash_at(Some(phase));
        let error = f.migrate("a").unwrap_err();
        crash_at(None);
        assert!(format!("{error:#}").contains("SIMULATED_CRASH"), "{phase}");
        let done = f.migrate("a").unwrap();
        assert_eq!(done["complete"], true, "{phase}: {done}");
        assert_all_trusted(&home);
        assert!(
            target(f.root(), "projects")
                .join(format!("{SLUG}/{CONVERSATION_A}.jsonl"))
                .exists()
        );
        assert_eq!(
            fs::read_dir(f.root().join(crate::layout::SHARED_MIGRATIONS))
                .unwrap()
                .count(),
            1,
            "{phase}"
        );
    }
}

#[test]
fn an_interrupted_exchange_placeholder_is_replaced_and_a_foreign_link_in_the_store_is_not() {
    require_exchange!();
    let mut f = fx();
    let home = f.account("a");
    seed(&home, CONVERSATION_A, "a");
    enable(f.root()).unwrap();
    // Our own placeholder: a link at the store path to itself.
    let z = target(f.root(), "projects");
    symlink(&z, &z).unwrap();
    let done = f.migrate("a").unwrap();
    assert_eq!(done["complete"], true, "{done}");
    assert!(z.is_dir() && !fs::symlink_metadata(&z).unwrap().file_type().is_symlink());

    let mut g = fx();
    let home = g.account("a");
    seed(&home, CONVERSATION_A, "a");
    enable(g.root()).unwrap();
    let elsewhere = g.root().join("elsewhere");
    crate::paths::private_dir(&elsewhere).unwrap();
    symlink(&elsewhere, target(g.root(), "projects")).unwrap();
    let error = format!("{:#}", g.migrate("a").unwrap_err());
    assert!(error.contains("SHARED_STATE_STORE_UNSAFE"), "{error}");
    assert!(home.join("projects").is_dir());
    assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
}

#[test]
fn a_foreign_link_in_the_account_path_is_refused_and_left_unchanged() {
    require_exchange!();
    let mut f = fx();
    let home = f.account("a");
    let elsewhere = f.root().join("elsewhere");
    write(&elsewhere.join("secret"), "s");
    symlink(&elsewhere, home.join("projects")).unwrap();
    let error = format!("{:#}", f.migrate("a").unwrap_err());
    assert!(error.contains("SHARED_STATE_LINK_UNTRUSTED"), "{error}");
    assert_eq!(fs::read_link(home.join("projects")).unwrap(), elsewhere);
    assert!(elsewhere.join("secret").exists());
}

#[test]
fn dry_run_changes_nothing() {
    let mut f = fx();
    let home = f.account("a");
    seed(&home, CONVERSATION_A, "a");
    let out = migrate::migrate(
        &f.paths,
        &Options {
            account: "a".into(),
            dry_run: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out["dry_run"], true);
    assert_eq!(out["names"]["projects"]["action"], "adopt_by_exchange");
    assert_eq!(out["names"]["projects"]["files"], 2);
    assert_eq!(out["would_complete"], true);
    assert!(fs::symlink_metadata(f.root().join("shared")).is_err());
    assert!(fs::symlink_metadata(f.root().join("backups")).is_err());
    assert!(fs::symlink_metadata(f.root().join(crate::layout::SHARED_MIGRATIONS)).is_err());
    assert!(
        fs::symlink_metadata(home.join("projects"))
            .unwrap()
            .is_dir()
    );
}

#[test]
fn backup_needs_reflink_unless_a_full_copy_is_allowed() {
    require_exchange!();
    let mut f = fx();
    let home = f.account("a");
    seed(&home, CONVERSATION_A, "a");
    let probe_dir = f._dir.path().join("probe");
    fs::create_dir(&probe_dir).unwrap();
    let reflinks = migrate::reflink_supported(&probe_dir);
    let result = migrate::migrate(
        &f.paths,
        &Options {
            account: "a".into(),
            ..Default::default()
        },
    );
    if reflinks {
        let out = result.unwrap();
        assert_eq!(out["backup_mode"], "reflink");
    } else {
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("BACKUP_REFLINK_UNSUPPORTED"), "{error}");
        assert!(error.contains("--allow-full-copy"));
        // Refused before anything changed.
        assert!(
            fs::symlink_metadata(home.join("projects"))
                .unwrap()
                .is_dir()
        );
        assert!(
            !fs::symlink_metadata(home.join("projects"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let out = f.migrate("a").unwrap();
        assert_eq!(out["backup_mode"], "includes_full_copies");
        assert_all_trusted(&home);
    }
}

#[test]
fn fs_path_does_the_io_but_link_targets_use_the_configured_root() {
    require_exchange!();
    let mut f = fx();
    let home = f.account("a");
    seed(&home, CONVERSATION_A, "a");
    // A different spelling of the same tree stands in for /mnt/disk1 vs /mnt/user.
    let alias = f._dir.path().join("disk");
    symlink(f.root(), &alias).unwrap();
    let out = migrate::migrate(
        &f.paths,
        &Options {
            account: "a".into(),
            allow_full_copy: true,
            fs_path: Some(alias.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out["complete"], true, "{out}");
    assert_eq!(out["alias_proof"], "probe_file");
    let stored = fs::read_link(home.join("projects")).unwrap();
    assert_eq!(stored, target(f.root(), "projects"));
    assert!(!stored.starts_with(&alias));
    assert_all_trusted(&home);

    // A path that is not the same tree is rejected before any change.
    let mut g = fx();
    let home = g.account("a");
    seed(&home, CONVERSATION_A, "a");
    let other = g._dir.path().join("other");
    fs::create_dir(&other).unwrap();
    let error = format!(
        "{:#}",
        migrate::migrate(
            &g.paths,
            &Options {
                account: "a".into(),
                allow_full_copy: true,
                fs_path: Some(other),
                ..Default::default()
            },
        )
        .unwrap_err()
    );
    assert!(error.contains("SHARED_STATE_FS_PATH_MISMATCH"), "{error}");
    assert!(home.join("projects").is_dir());
    assert!(!trusted_link(&home.join("projects")));
}

#[test]
fn switching_needs_both_accounts_linked_or_both_plain() {
    let mut f = fx();
    let a = f.account("a");
    let b = f.account("b");
    assert!(!switch_shared(&f.paths, &a, &b).unwrap(), "feature off");
    enable(f.root()).unwrap();
    assert!(switch_shared(&f.paths, &a, &b).unwrap());
    // Half linked (the second account lost one link to a data directory).
    fs::remove_file(b.join("todos")).unwrap();
    write(&b.join("todos/x.json"), "[]");
    let error = switch_shared(&f.paths, &a, &b).unwrap_err().to_string();
    assert!(error.starts_with("SHARED_STATE_UNMIGRATED"), "{error}");
    // A plain home next to a linked one would make the old move delete from the store.
    let adopted = f._dir.path().join("adopted");
    crate::paths::private_dir(&adopted).unwrap();
    let error = switch_shared(&f.paths, &a, &adopted)
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("SHARED_STATE_MIXED"), "{error}");
}

#[test]
fn stamp_formats_utc_seconds() {
    assert_eq!(migrate::stamp(0), "19700101T000000Z");
    assert_eq!(migrate::stamp(1_793_000_000), "20261026T073320Z");
}
