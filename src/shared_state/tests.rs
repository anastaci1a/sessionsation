use super::*;
use crate::{
    model::{Account, State},
    store::Store,
};
use std::{cell::RefCell, os::unix::fs::PermissionsExt};

thread_local! {
    static CRASH_AT: RefCell<Option<&'static str>> = const { RefCell::new(None) };
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
    fn root(&self) -> &Path {
        &self.paths.root
    }
}

const CONVERSATION_A: &str = "11111111-1111-4111-8111-111111111111";
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
            created: NAMES.to_vec(),
            unmigrated: vec![]
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
}

#[test]
fn ensure_links_creates_repairs_and_refuses_data_and_foreign_links() {
    let mut f = fx();
    let home = f.account("a");
    enable(f.root()).unwrap();
    // An empty real directory is replaced; the others are created.
    fs::create_dir(home.join("todos")).unwrap();
    let Ensure::Linked { created, .. } = ensure_links(&f.paths, &home).unwrap() else {
        panic!()
    };
    assert_eq!(created, NAMES.to_vec());
    assert_all_trusted(&home);
    assert_eq!(
        ensure_links(&f.paths, &home).unwrap(),
        Ensure::Linked {
            created: vec![],
            unmigrated: vec![]
        },
        "idempotent"
    );
    // A deleted link is repaired.
    fs::remove_file(home.join("projects")).unwrap();
    assert_eq!(
        ensure_links(&f.paths, &home).unwrap(),
        Ensure::Linked {
            created: vec!["projects"],
            unmigrated: vec![]
        }
    );

    // A real directory with data is never merged here.
    let b = f.account("b");
    seed(&b, CONVERSATION_A, "b");
    let error = ensure_links(&f.paths, &b).unwrap_err().to_string();
    assert!(error.starts_with("SHARED_STATE_UNMIGRATED"), "{error}");
    assert!(error.contains("layout-3 root"), "{error}");
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
    assert_eq!(
        removed,
        vec![
            "projects",
            "file-history",
            "session-env",
            "agents",
            "commands"
        ]
    );
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

// ------------------------------------------- agents, skills and commands (1.46.0)

#[test]
fn custom_names_are_shared_status_reports_them_and_settings_stay_per_account() {
    let mut f = fx();
    let home = f.account("a");
    write(&home.join("settings.json"), "{}");
    enable(f.root()).unwrap();
    assert_eq!(CUSTOM, ["agents", "commands"]);
    assert!(!NAMES.contains(&"skills"));
    ensure_links(&f.paths, &home).unwrap();
    for n in CUSTOM {
        assert_eq!(state_of(&home, n), LinkState::Linked);
        assert!(trusted_link(&home.join(n)));
    }
    assert!(
        fs::symlink_metadata(home.join("settings.json"))
            .unwrap()
            .is_file()
    );
    let st = account_status(&f.paths, "a", &home);
    for n in CUSTOM {
        assert_eq!(st["names"][n], "linked");
    }
    assert_eq!(st["linked"], true);
    // A real directory with data does not block ensure_links for the new names.
    let mut g = fx();
    let h = g.account("b");
    enable(g.root()).unwrap();
    write(&h.join("agents/x.md"), "x");
    let Ensure::Linked { unmigrated, .. } = ensure_links(&g.paths, &h).unwrap() else {
        panic!()
    };
    assert_eq!(unmigrated, vec!["agents"]);
    assert_eq!(state_of(&h, "agents"), LinkState::Data);
    assert_eq!(
        account_status(&g.paths, "b", &h)["names"]["agents"],
        "unmigrated"
    );
    assert_eq!(account_status(&g.paths, "b", &h)["linked"], false);
}

#[test]
fn a_new_account_gets_the_custom_links_when_shared_state_is_enabled() {
    let mut f = fx();
    enable(f.root()).unwrap();
    write(&target(f.root(), "agents").join("builder.md"), "builder");
    let work = f.account("work");
    // What the daemon does when an account is added.
    ensure_links(&f.paths, &work).unwrap();
    for n in CUSTOM {
        assert!(trusted_link(&work.join(n)), "{n}");
    }
    assert_eq!(
        fs::read_to_string(work.join("agents/builder.md")).unwrap(),
        "builder"
    );
}
