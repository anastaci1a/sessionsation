use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn keys_are_validated_trimmed_and_never_formatted() {
    let key = ApiKey::new("  AIzaFAKE-key_123\n").unwrap();
    assert_eq!(key.expose(), "AIzaFAKE-key_123");
    assert!(!format!("{key:?} {key}").contains("AIza"));
    for bad in ["", "   ", "has space", "tab\tkey", "ключ"] {
        let e = ApiKey::new(bad).unwrap_err().to_string();
        assert!(e.starts_with("INVALID_API_KEY_FORMAT"), "{e}");
        assert!(bad.trim().is_empty() || !e.contains(bad.trim()));
    }
    assert!(ApiKey::new(&"a".repeat(MAX_KEY + 1)).is_err());
}

#[test]
fn key_file_is_private_atomic_and_refuses_symlinks() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    fs::create_dir(&home).unwrap();
    assert!(!key_present(&home) && read_key(&home).is_none());
    let key = ApiKey::new("AIzaFAKE1").unwrap();
    store_key(&home, &key).unwrap();
    let dir_mode = fs::metadata(secrets_dir(&home))
        .unwrap()
        .permissions()
        .mode();
    let file_mode = fs::metadata(key_path(&home)).unwrap().permissions().mode();
    assert_eq!((dir_mode & 0o777, file_mode & 0o777), (0o700, 0o600));
    assert_eq!(read_key(&home), Some(key));
    // No temp files are left beside the key.
    assert_eq!(fs::read_dir(secrets_dir(&home)).unwrap().count(), 1);
    // A group/world-readable key file is not used.
    fs::set_permissions(key_path(&home), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!key_present(&home) && read_key(&home).is_none());
    fs::set_permissions(key_path(&home), fs::Permissions::from_mode(0o600)).unwrap();
    // A symlinked key file is refused, never followed, and left unchanged.
    let outside = d.path().join("outside");
    fs::write(&outside, "elsewhere").unwrap();
    fs::remove_file(key_path(&home)).unwrap();
    std::os::unix::fs::symlink(&outside, key_path(&home)).unwrap();
    assert!(!key_present(&home) && read_key(&home).is_none());
    let e = store_key(&home, &ApiKey::new("AIzaFAKE2").unwrap()).unwrap_err();
    assert!(e.to_string().contains("UNSAFE_SECRET_PATH"), "{e}");
    assert!(!format!("{e:#}").contains("AIzaFAKE2"));
    assert_eq!(fs::read_to_string(&outside).unwrap(), "elsewhere");
    // A symlinked secrets directory is refused too.
    let other = tempfile::tempdir().unwrap();
    let home2 = d.path().join("home2");
    fs::create_dir(&home2).unwrap();
    std::os::unix::fs::symlink(other.path(), secrets_dir(&home2)).unwrap();
    assert!(store_key(&home2, &ApiKey::new("AIzaFAKE3").unwrap()).is_err());
    assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
    // Removal.
    fs::remove_file(key_path(&home)).unwrap();
    store_key(&home, &ApiKey::new("AIzaFAKE4").unwrap()).unwrap();
    assert!(remove_key(&home).unwrap());
    assert!(!remove_key(&home).unwrap());
}

#[test]
fn mode_switching_updates_selected_type_and_child_env() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    super::super::seed_settings(home).unwrap();
    assert_eq!(mode(home), Mode::Google);
    assert!(child_env(home).is_empty());
    store_key(home, &ApiKey::new("AIzaFAKE5").unwrap()).unwrap();
    assert!(child_env(home).is_empty(), "google mode never passes a key");
    assert!(set_mode(home, Mode::ApiKey).unwrap());
    assert!(!set_mode(home, Mode::ApiKey).unwrap(), "unchanged");
    assert_eq!(mode(home), Mode::ApiKey);
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".gemini/settings.json")).unwrap()).unwrap();
    assert_eq!(settings["security"]["auth"]["selectedType"], API_KEY_TYPE);
    assert_eq!(settings["general"]["enableAutoUpdate"], false);
    assert!(!settings.to_string().contains("AIzaFAKE5"));
    let env = child_env(home);
    assert_eq!(
        env.iter()
            .map(|(k, v)| (*k, v.expose()))
            .collect::<Vec<_>>(),
        [("GEMINI_API_KEY", "AIzaFAKE5"), (WRAPPER_ENV, "AIzaFAKE5")]
    );
    // Re-seeding (every launch) keeps the operator's chosen mode.
    super::super::seed_settings(home).unwrap();
    assert_eq!(mode(home), Mode::ApiKey);
    assert!(set_mode(home, Mode::Google).unwrap());
    assert_eq!(mode(home), Mode::Google);
    assert!(child_env(home).is_empty());
    assert!(
        Mode::parse("vertex")
            .unwrap_err()
            .to_string()
            .starts_with("UNSUPPORTED_AUTH_MODE")
    );
    assert_eq!(Mode::parse("api-key").unwrap(), Mode::ApiKey);
}
