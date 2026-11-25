use super::*;
#[tokio::test]
async fn portable_publication_preserves_files_and_never_merges() {
    let t = tempfile::tempdir().unwrap();
    let from = t.path().join("staging");
    let to = t.path().join("destination");
    fs::create_dir(&from).unwrap();
    fs::write(from.join("file"), "unsaved changes").unwrap();
    std::os::unix::fs::symlink("file", from.join("link")).unwrap();
    copy_new_directory(&from, &to).await.unwrap();
    assert_eq!(fs::read(to.join("file")).unwrap(), b"unsaved changes");
    assert_eq!(fs::read_link(to.join("link")).unwrap(), Path::new("file"));
    assert!(from.join("file").exists());
    fs::write(from.join("file"), "different").unwrap();
    assert!(copy_new_directory(&from, &to).await.is_err());
    assert_eq!(fs::read(to.join("file")).unwrap(), b"unsaved changes");
}
#[test]
fn missing_parents_resolve_without_writes_and_reject_broken_symlinks() {
    use std::os::unix::fs::symlink;
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("new/nested/project");
    assert_eq!(destination(&target).unwrap(), target);
    assert!(!target.parent().unwrap().exists());
    symlink(t.path().join("absent"), t.path().join("broken")).unwrap();
    assert!(destination(&t.path().join("broken/project")).is_err());
    assert!(destination(&t.path().join("new/../project")).is_err());
}
#[test]
fn only_known_background_service_commands_can_ignore_inherited_cwd() {
    let service =
        |argv: &[&str]| cwd_only_service(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(
        service(&["/bin/codex", "app-server", "daemon", "pid-update-loop"]),
        Some(Service::PidUpdater)
    );
    assert_eq!(
        service(&[
            "adb",
            "-L",
            "tcp:5037",
            "fork-server",
            "server",
            "--reply-fd",
            "4"
        ]),
        Some(Service::Adb)
    );
    assert_eq!(service(&["adb", "install", "project.apk"]), None);
    assert_eq!(service(&["codex", "resume", "thread"]), None);
    assert_eq!(
        service(&["other", "app-server", "daemon", "pid-update-loop"]),
        None
    );
}
#[test]
fn atomic_rename_never_replaces_existing_destination() {
    let t = tempfile::tempdir().unwrap();
    let from = t.path().join("from");
    let to = t.path().join("to");
    fs::create_dir(&from).unwrap();
    fs::create_dir(&to).unwrap();
    assert!(rename_new(&from, &to).is_err());
    assert!(from.exists() && to.exists());
}
