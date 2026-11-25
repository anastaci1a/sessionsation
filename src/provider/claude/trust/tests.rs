use super::*;
use serde_json::{Value, json};

fn pretty(v: &Value) -> String {
    // serde_json's pretty printer matches JSON.stringify(_, null, 2).
    serde_json::to_string_pretty(v).unwrap()
}

#[test]
fn writes_only_the_trust_key_and_keeps_every_other_byte() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let d = fs::canonicalize(dir.path()).unwrap();
    let d = d.to_str().unwrap();
    let original = json!({
        "numStartups": 12,
        "zeta": {"nested": [1, 2.50, {"x": null}]},
        "projects": {
            "/other": {"allowedTools": ["Bash"], "hasTrustDialogAccepted": true},
            d: {"allowedTools": [], "lastCost": 1.25, "hasTrustDialogAccepted": false}
        },
        "oauthAccount": {"emailAddress": "kept@example.invalid"},
        "alpha": "last"
    });
    // Byte-exact number spelling and key order must survive.
    let text = pretty(&original).replace("2.5", "2.50") + "\n";
    let file = home.path().join(CONFIG_FILE);
    fs::write(&file, &text).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    fs::write(home.path().join(".credentials.json"), "secret").unwrap();
    fs::set_permissions(
        home.path().join(".credentials.json"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();

    assert_eq!(pretrust(home.path(), dir.path()).unwrap(), Outcome::Trusted);
    let after = fs::read_to_string(&file).unwrap();
    assert_eq!(
        after,
        text.replace(
            "\"hasTrustDialogAccepted\": false",
            "\"hasTrustDialogAccepted\": true"
        )
    );
    assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o640);
    assert_eq!(
        fs::read(home.path().join(".credentials.json")).unwrap(),
        b"secret"
    );
    // Idempotent: a trusted directory is never rewritten.
    let ino = fs::metadata(&file).unwrap().ino();
    assert_eq!(
        pretrust(home.path(), dir.path()).unwrap(),
        Outcome::AlreadyTrusted
    );
    assert_eq!(fs::metadata(&file).unwrap().ino(), ino);
    // No temporary files are left beside the config.
    let names: Vec<_> = fs::read_dir(home.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.contains("pretrust")), "{names:?}");
}

#[test]
fn unknown_directory_gets_claude_defaults_and_order_is_kept() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let d = fs::canonicalize(dir.path()).unwrap();
    // Literal text: unsorted keys, as Claude writes them.
    let text = "{\n  \"b\": 1,\n  \"a\": {\n    \"projects\": \"not top\"\n  },\n  \"projects\": {\n    \"/x\": {}\n  }\n}";
    fs::write(home.path().join(CONFIG_FILE), text).unwrap();
    pretrust(home.path(), dir.path()).unwrap();
    let after = fs::read_to_string(home.path().join(CONFIG_FILE)).unwrap();
    let v: Value = serde_json::from_str(&after).unwrap();
    let entry = &v["projects"][d.to_str().unwrap()];
    assert_eq!(entry["hasTrustDialogAccepted"], true);
    assert_eq!(entry["allowedTools"], json!([]));
    assert_eq!(v["projects"]["/x"], json!({}));
    assert_eq!(v["a"], json!({"projects": "not top"}));
    assert!(after.starts_with("{\n  \"b\": 1,\n  \"a\""), "{after}");
    // Claude's JSON.stringify(_, null, 2) layout: the old text is a prefix.
    let kept = "{\n  \"b\": 1,\n  \"a\": {\n    \"projects\": \"not top\"\n  },\n  \"projects\": {\n    \"/x\": {},\n    ";
    assert!(after.starts_with(kept), "{after}");
    assert!(after.contains("\n      \"allowedTools\": [],\n"), "{after}");
    assert!(after.ends_with("\n    }\n  }\n}"), "{after}");
}

#[test]
fn config_without_projects_gains_one_entry() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(home.path().join(CONFIG_FILE), "{\"numStartups\":1}").unwrap();
    pretrust(home.path(), dir.path()).unwrap();
    let v: Value =
        serde_json::from_slice(&fs::read(home.path().join(CONFIG_FILE)).unwrap()).unwrap();
    assert_eq!(v["numStartups"], 1);
    let key = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        v["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
        true
    );
}

#[test]
fn refuses_symlinks_and_never_touches_bad_or_missing_files() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Missing: nothing is created.
    assert_eq!(
        pretrust(home.path(), dir.path()).unwrap(),
        Outcome::NoConfig
    );
    assert!(!home.path().join(CONFIG_FILE).exists());
    // Symlink: refused, target unchanged.
    let target = home.path().join("real.json");
    fs::write(&target, "{}").unwrap();
    std::os::unix::fs::symlink(&target, home.path().join(CONFIG_FILE)).unwrap();
    let err = pretrust(home.path(), dir.path()).unwrap_err();
    assert!(
        format!("{err:#}").contains("CLAUDE_CONFIG_SYMLINK"),
        "{err:#}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
    fs::remove_file(home.path().join(CONFIG_FILE)).unwrap();
    // Unparsable or wrongly shaped: refused, unchanged.
    for bad in [
        "{not json",
        "[]",
        "{\"projects\": []}",
        "{\"projects\": {\"KEY\": 3}}",
    ] {
        let key = fs::canonicalize(dir.path()).unwrap();
        let bad = bad.replace("KEY", key.to_str().unwrap());
        fs::write(home.path().join(CONFIG_FILE), &bad).unwrap();
        assert!(pretrust(home.path(), dir.path()).is_err(), "{bad}");
        assert_eq!(
            fs::read_to_string(home.path().join(CONFIG_FILE)).unwrap(),
            bad
        );
    }
}

#[test]
fn symlinked_directory_is_trusted_under_both_spellings() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let link = home.path().join("link");
    std::os::unix::fs::symlink(dir.path(), &link).unwrap();
    fs::write(home.path().join(CONFIG_FILE), "{}").unwrap();
    pretrust(home.path(), &link).unwrap();
    let v: Value =
        serde_json::from_slice(&fs::read(home.path().join(CONFIG_FILE)).unwrap()).unwrap();
    let real = fs::canonicalize(dir.path()).unwrap();
    for key in [real.to_str().unwrap(), link.to_str().unwrap()] {
        assert_eq!(v["projects"][key]["hasTrustDialogAccepted"], true, "{key}");
    }
}
