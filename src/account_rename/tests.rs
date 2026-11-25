use super::*;
use crate::model::Account;

struct Fixture {
    _dir: tempfile::TempDir,
    paths: Paths,
    state: State,
    store: Store,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let store = Store::open(&paths).unwrap();
    Fixture {
        _dir: dir,
        paths,
        state: State::default(),
        store,
    }
}
impl Fixture {
    fn enroll(&mut self, provider: ProviderId, name: &str, display: Option<&str>) -> String {
        let mut a = Account::with_provider(provider, name.into(), String::new());
        a.display_name = display.map(str::to_owned);
        self.state.insert_account(a);
        self.store.save(&self.state, 1, "test", name).unwrap();
        self.state
            .account(provider, name)
            .map(account_generation)
            .unwrap()
    }
    fn home(&self, provider: ProviderId, name: &str) -> PathBuf {
        let home = crate::layout::target_home(&self.paths.root, provider, name);
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("history.txt"), b"kept").unwrap();
        home
    }
    fn plan(
        &self,
        provider: ProviderId,
        name: &str,
        new: Option<&str>,
        generation: &str,
    ) -> Result<Plan> {
        plan(&self.paths, &self.state, provider, name, new, generation)
    }
    fn run(&mut self, plan: Plan, id: &str, crash_after: Option<&'static str>) -> Result<Value> {
        let hook = move |step: &str| {
            ensure!(Some(step) != crash_after, "SIMULATED_CRASH at {step}");
            Ok(())
        };
        execute(
            &self.paths,
            &mut self.state,
            &mut self.store,
            plan,
            id,
            &hook,
        )
    }
}
const ID: &str = "11111111-1111-4111-8111-111111111111";

#[test]
fn rename_moves_home_registry_and_sessions_and_keeps_history() {
    let mut f = fixture();
    let generation = f.enroll(ProviderId::Codex, "personal", Some("Office"));
    f.enroll(ProviderId::Claude, "personal", None);
    let home = f.home(ProviderId::Codex, "personal");
    crate::catalog::record_provider_launch(
        &f.paths,
        ProviderId::Codex,
        "personal",
        "c1",
        "one",
        None,
        None,
    )
    .unwrap();
    crate::catalog::record_provider_launch(
        &f.paths,
        ProviderId::Claude,
        "personal",
        "c2",
        "two",
        None,
        None,
    )
    .unwrap();
    let plan = f
        .plan(ProviderId::Codex, "personal", Some("work2"), &generation)
        .unwrap();
    let out = f.run(plan, ID, None).unwrap();
    assert_eq!(out["phase"], "complete");
    let new_home = crate::layout::target_home(&f.paths.root, ProviderId::Codex, "work2");
    assert!(!home.exists());
    assert_eq!(fs::read(new_home.join("history.txt")).unwrap(), b"kept");
    let a = f.state.account(ProviderId::Codex, "work2").unwrap();
    // The display name differed from the new name, so it stays a mismatch.
    assert_eq!(a.display_name.as_deref(), Some("Office"));
    assert!(f.state.account(ProviderId::Codex, "personal").is_none());
    // The same-named Claude account is untouched, and so are its sessions.
    assert!(f.state.account(ProviderId::Claude, "personal").is_some());
    let sessions = crate::catalog::list(&f.paths).unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s.provider == "codex" && s.account == "work2")
    );
    assert!(
        sessions
            .iter()
            .any(|s| s.provider == "claude" && s.account == "personal")
    );
    // The registry on disk agrees.
    let persisted = f.store.load().unwrap();
    assert!(persisted.account(ProviderId::Codex, "work2").is_some());
}

#[test]
fn update_backend_without_a_name_matches_the_display_name() {
    let mut f = fixture();
    let generation = f.enroll(ProviderId::Claude, "old", Some("Studio"));
    f.home(ProviderId::Claude, "old");
    let plan = f
        .plan(ProviderId::Claude, "old", None, &generation)
        .unwrap();
    assert_eq!(plan.new_name, "Studio");
    f.run(plan, ID, None).unwrap();
    let a = f.state.account(ProviderId::Claude, "Studio").unwrap();
    assert_eq!(
        a.display_name, None,
        "folder name == display name afterwards"
    );
    assert!(crate::layout::target_home(&f.paths.root, ProviderId::Claude, "Studio").is_dir());
    // Without a display name there is nothing to match.
    let g = f.enroll(ProviderId::Gemini, "plain", None);
    let e = f
        .plan(ProviderId::Gemini, "plain", None, &g)
        .unwrap_err()
        .to_string();
    assert!(e.contains("INVALID_ARGUMENT"), "{e}");
}

#[test]
fn a_deferred_legacy_home_moves_to_the_layout_2_path() {
    let mut f = fixture();
    let generation = f.enroll(ProviderId::Claude, "legacy", None);
    let legacy = f.paths.root.join("claude-accounts/legacy");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("history.txt"), b"old").unwrap();
    assert_eq!(
        crate::layout::account_home(&f.paths.root, ProviderId::Claude, "legacy"),
        legacy
    );
    let plan = f
        .plan(ProviderId::Claude, "legacy", Some("fresh"), &generation)
        .unwrap();
    f.run(plan, ID, None).unwrap();
    assert!(!legacy.exists());
    let target = f.paths.root.join("accounts/claude/fresh");
    assert_eq!(fs::read(target.join("history.txt")).unwrap(), b"old");
    assert_eq!(
        crate::layout::account_home(&f.paths.root, ProviderId::Claude, "fresh"),
        target
    );
}

#[test]
fn collisions_are_per_provider_and_exact_case() {
    let mut f = fixture();
    let generation = f.enroll(ProviderId::Codex, "a", None);
    f.enroll(ProviderId::Codex, "taken", None);
    f.enroll(ProviderId::Claude, "free", None);
    f.enroll(ProviderId::Codex, "other", Some("shown"));
    f.home(ProviderId::Codex, "a");
    let e = f
        .plan(ProviderId::Codex, "a", Some("taken"), &generation)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("ACCOUNT_NAME_EXISTS"), "{e}");
    // Another account's display name of the same provider also collides.
    let e = f
        .plan(ProviderId::Codex, "a", Some("shown"), &generation)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("ACCOUNT_NAME_EXISTS"), "{e}");
    // The same name under another provider is no collision.
    assert!(
        f.plan(ProviderId::Codex, "a", Some("free"), &generation)
            .is_ok()
    );
    // Case is significant, like account creation.
    assert!(
        f.plan(ProviderId::Codex, "a", Some("Taken"), &generation)
            .is_ok()
    );
    let e = f
        .plan(ProviderId::Codex, "a", Some("a"), &generation)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("ACCOUNT_NAME_UNCHANGED"), "{e}");
    // A stale generation, an existing folder and an adopted home are refused.
    let e = f
        .plan(ProviderId::Codex, "a", Some("fresh"), "0000")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("STALE_TARGET"), "{e}");
    f.home(ProviderId::Codex, "occupied");
    let e = f
        .plan(ProviderId::Codex, "a", Some("occupied"), &generation)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("ACCOUNT_HOME_EXISTS"), "{e}");
    let mut adopted = Account::with_provider(ProviderId::Claude, "ext".into(), String::new());
    adopted.config_home = Some("/somewhere/else".into());
    f.state.insert_account(adopted);
    let g = account_generation(f.state.account(ProviderId::Claude, "ext").unwrap());
    let e = f
        .plan(ProviderId::Claude, "ext", Some("inside"), &g)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("ACCOUNT_HOME_ADOPTED"), "{e}");
}

#[test]
fn a_running_session_blocks_the_rename_under_the_operation_lock() {
    let mut f = fixture();
    let generation = f.enroll(ProviderId::Claude, "busy", None);
    f.home(ProviderId::Claude, "busy");
    let plan = f
        .plan(ProviderId::Claude, "busy", Some("quiet"), &generation)
        .unwrap();
    // A catalog session whose recorded process is this (live) test process.
    let me = std::process::id();
    let start = crate::external::process_start_time(me).unwrap();
    crate::catalog::record_provider_launch(
        &f.paths,
        ProviderId::Claude,
        "busy",
        "conv",
        "s",
        None,
        Some(crate::catalog::Attachment {
            host: "tmux".into(),
            target: "%1".into(),
            pid: me,
            process_start: start,
        }),
    )
    .unwrap();
    let blocked = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(ensure_idle(&f.paths, &plan))
        .unwrap_err()
        .to_string();
    assert!(blocked.starts_with("ACCOUNT_SESSIONS_RUNNING"), "{blocked}");
    // The account operation lock excludes a concurrent rename or removal.
    let _held = accounts::operation_lock(&f.paths, ProviderId::Claude, "busy").unwrap();
    assert!(accounts::operation_lock(&f.paths, ProviderId::Claude, "busy").is_err());
    // The same name under another provider uses another lock.
    assert!(accounts::operation_lock(&f.paths, ProviderId::Codex, "busy").is_ok());
}

#[test]
fn crash_before_the_move_reconciles_to_failed_without_changes() {
    for step in ["admitted", "moving_home"] {
        let mut f = fixture();
        let g = f.enroll(ProviderId::Codex, "p", None);
        let home = f.home(ProviderId::Codex, "p");
        let plan = f.plan(ProviderId::Codex, "p", Some("q"), &g).unwrap();
        assert!(f.run(plan, ID, Some(step)).is_err());
        assert!(!is_terminal(&read(&f.paths, ID).unwrap()));
        // The unfinished rename blocks launches and further renames of either name.
        assert!(
            accounts::ensure_available(&f.paths, ProviderId::Codex, "p")
                .unwrap_err()
                .to_string()
                .contains("ACCOUNT_RENAME_UNRESOLVED")
        );
        let out = reconcile(&f.paths, &mut f.state, &mut f.store, ID, &|_| Ok(())).unwrap();
        assert_eq!(out["phase"], "failed", "{step}");
        assert!(home.is_dir());
        assert!(f.state.account(ProviderId::Codex, "p").is_some());
        assert!(accounts::ensure_available(&f.paths, ProviderId::Codex, "p").is_ok());
    }
}

#[test]
fn crash_after_the_move_rolls_forward_deterministically() {
    for step in ["home_moved", "registry_updated"] {
        let mut f = fixture();
        let g = f.enroll(ProviderId::Gemini, "p", Some("Shown"));
        f.home(ProviderId::Gemini, "p");
        crate::catalog::record_provider_launch(
            &f.paths,
            ProviderId::Gemini,
            "p",
            "c",
            "s",
            None,
            None,
        )
        .unwrap();
        let plan = f.plan(ProviderId::Gemini, "p", Some("q"), &g).unwrap();
        assert!(f.run(plan, ID, Some(step)).is_err());
        // Simulate the supervisor restarting: registry state comes from disk.
        f.state = f.store.load().unwrap();
        let out = reconcile(&f.paths, &mut f.state, &mut f.store, ID, &|_| Ok(())).unwrap();
        assert_eq!(out["phase"], "complete", "{step}");
        let persisted = f.store.load().unwrap();
        assert!(persisted.account(ProviderId::Gemini, "q").is_some());
        assert!(persisted.account(ProviderId::Gemini, "p").is_none());
        assert!(crate::layout::target_home(&f.paths.root, ProviderId::Gemini, "q").is_dir());
        assert_eq!(crate::catalog::list(&f.paths).unwrap()[0].account, "q");
        // Reconciling a finished operation changes nothing.
        let again = reconcile(&f.paths, &mut f.state, &mut f.store, ID, &|_| Ok(())).unwrap();
        assert_eq!(again["phase"], "complete");
    }
}

#[test]
fn an_undecidable_state_is_uncertain_and_can_only_be_abandoned() {
    let mut f = fixture();
    let g = f.enroll(ProviderId::Codex, "p", None);
    f.home(ProviderId::Codex, "p");
    let plan = f.plan(ProviderId::Codex, "p", Some("q"), &g).unwrap();
    assert!(f.run(plan, ID, Some("home_moved")).is_err());
    // Both folders exist: the move cannot be told apart from a copy.
    f.home(ProviderId::Codex, "p");
    let out = reconcile(&f.paths, &mut f.state, &mut f.store, ID, &|_| Ok(())).unwrap();
    assert_eq!(out["phase"], "uncertain");
    assert_eq!(out["uncertain"], true);
    assert!(
        abandon(&f.paths, ID, "other")
            .unwrap_err()
            .to_string()
            .contains("CONFIRMATION_MISMATCH")
    );
    let done = abandon(&f.paths, ID, ID).unwrap();
    assert_eq!(done["phase"], "abandoned");
    assert!(
        abandon(&f.paths, ID, ID)
            .unwrap_err()
            .to_string()
            .contains("OPERATION_ALREADY_TERMINAL")
    );
    // Abandoned releases the account for further work.
    assert!(accounts::ensure_available(&f.paths, ProviderId::Codex, "p").is_ok());
    // Nothing is replayed: the same id cannot run again.
    let plan = f
        .plan(
            ProviderId::Codex,
            "p",
            Some("r"),
            &account_generation(f.state.account(ProviderId::Codex, "p").unwrap()),
        )
        .unwrap();
    assert!(
        f.run(plan, ID, None)
            .unwrap_err()
            .to_string()
            .contains("OPERATION_ALREADY_EXISTS")
    );
}

#[test]
fn a_failed_move_leaves_everything_unchanged() {
    let mut f = fixture();
    let g = f.enroll(ProviderId::Codex, "p", None);
    // No home on disk: the move cannot start.
    let plan = f.plan(ProviderId::Codex, "p", Some("q"), &g).unwrap();
    let e = f.run(plan, ID, None).unwrap_err().to_string();
    assert!(e.contains("ACCOUNT_HOME_UNAVAILABLE"), "{e}");
    assert_eq!(read(&f.paths, ID).unwrap().phase, "failed");
    assert!(f.state.account(ProviderId::Codex, "p").is_some());
    assert!(accounts::ensure_available(&f.paths, ProviderId::Codex, "p").is_ok());
}
