use super::*;

fn setup() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = super::super::state_dir(home.path());
    fs::create_dir_all(&state).unwrap();
    let file = state.join("settings.json");
    (home, dir, file)
}

#[test]
fn appends_one_element_and_keeps_every_other_byte() {
    let (home, dir, file) = setup();
    let d = fs::canonicalize(dir.path()).unwrap();
    let text = "{\n  \"queuedMessages\": \"queue\",\n  \"nested\": {\"x\": [1, 2.50, null]},\n  \"trustedWorkspaces\": [\n    \"/root\"\n  ],\n  \"tail\": 1\n}\n";
    fs::write(&file, text).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    let token = super::super::state_dir(home.path()).join("antigravity-oauth-token");
    fs::write(&token, "secret").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o000)).unwrap();

    assert_eq!(pretrust(home.path(), dir.path()).unwrap(), Outcome::Trusted);
    let after = fs::read_to_string(&file).unwrap();
    let expected = text.replace(
        "    \"/root\"\n",
        &format!("    \"/root\",\n    \"{}\"\n", d.display()),
    );
    assert_eq!(after, expected);
    assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o640);
    assert_eq!(
        fs::read_dir(super::super::state_dir(home.path()))
            .unwrap()
            .count(),
        2,
        "only settings.json and the untouched token remain"
    );
    // Idempotent.
    assert_eq!(
        pretrust(home.path(), dir.path()).unwrap(),
        Outcome::AlreadyTrusted
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), after);
}

#[test]
fn creates_the_list_and_the_file_when_missing() {
    let (home, dir, file) = setup();
    fs::write(&file, "{\n  \"queuedMessages\": \"send\"\n}").unwrap();
    pretrust(home.path(), dir.path()).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(v["queuedMessages"], "send");
    assert_eq!(v[KEY].as_array().unwrap().len(), 1);

    fs::remove_file(&file).unwrap();
    pretrust(home.path(), dir.path()).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(v[KEY].as_array().unwrap().len(), 1);
    assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn refuses_symlinks_and_leaves_unparsable_files_alone() {
    let (home, dir, file) = setup();
    let real = home.path().join("real.json");
    fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, &file).unwrap();
    let e = pretrust(home.path(), dir.path()).unwrap_err().to_string();
    assert!(e.contains("AGY_SETTINGS_SYMLINK"), "{e}");
    assert_eq!(fs::read_to_string(&real).unwrap(), "{}");

    fs::remove_file(&file).unwrap();
    fs::write(&file, "not json").unwrap();
    assert!(pretrust(home.path(), dir.path()).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), "not json");

    fs::write(&file, "{\"trustedWorkspaces\": 5}").unwrap();
    assert!(pretrust(home.path(), dir.path()).is_err());
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "{\"trustedWorkspaces\": 5}"
    );
}

#[test]
fn a_missing_state_directory_is_not_created() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert!(pretrust(home.path(), dir.path()).is_err());
    assert!(!super::super::state_dir(home.path()).exists());
}
