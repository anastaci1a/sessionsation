use super::*;

#[test]
fn the_owner_marker_disables_native_sending_even_as_a_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("root"))).unwrap();
    assert!(sending_enabled(&paths).unwrap());
    let marker = paths.root.join("messaging/native-send.disabled");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&marker, "").unwrap();
    assert!(!sending_enabled(&paths).unwrap());
    fs::remove_file(&marker).unwrap();
    std::os::unix::fs::symlink("/nonexistent", &marker).unwrap();
    assert!(!sending_enabled(&paths).unwrap());
}
