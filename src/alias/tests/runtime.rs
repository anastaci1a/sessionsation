use super::super::{Entry, runtime};
use crate::{
    layout::{self, RootAlias},
    paths::Paths,
};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::PathBuf,
};

#[test]
fn runtime_compatibility_links_reach_the_canonical_socket_and_are_safe() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("disk/appdata/root"))).unwrap();
    let old_root = t.path().join("user/appdata/root");
    let entries: Vec<Entry> = ["appdata", "live"]
        .iter()
        .map(|s| Entry {
            from: t.path().join("user").join(s),
            to: t.path().join("disk").join(s),
            proof: "test".into(),
        })
        .collect();
    let mut record = layout::read_record(&paths.root).unwrap().unwrap();
    record.version = layout::ALIAS_VERSION;
    record.root_alias = Some(RootAlias {
        version: 1,
        state: "active".into(),
        root_id: "r".into(),
        canonical_root: paths.root.clone(),
        previous_root: old_root.clone(),
        entries,
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
    drop(listener);
    // A stale private directory is moved aside (never removed); a live socket is refused.
    let stale = t.path().join("stale");
    fs::create_dir(&stale).unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(stale.join("old.txt"), "kept").unwrap();
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
