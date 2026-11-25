use super::super::*;

fn alias() -> Alias {
    Alias::new(vec![
        Entry {
            from: "/mnt/user/appdata".into(),
            to: "/mnt/disk1/appdata".into(),
            proof: "same_inode".into(),
        },
        Entry {
            from: "/mnt/user/live".into(),
            to: "/mnt/disk1/live".into(),
            proof: "same_inode".into(),
        },
        Entry {
            from: "/mnt/user/live/deep".into(),
            to: "/mnt/disk2/deep".into(),
            proof: "same_inode".into(),
        },
    ])
}

#[test]
fn logical_table() {
    let a = alias();
    for (input, want) in [
        ("/mnt/user/appdata", "/mnt/disk1/appdata"),
        ("/mnt/user/appdata/", "/mnt/disk1/appdata"),
        (
            "/mnt/user/appdata/sessionsation/x.json",
            "/mnt/disk1/appdata/sessionsation/x.json",
        ),
        // Component boundaries: a sibling with the same text prefix is not mapped.
        ("/mnt/user/appdata2/x", "/mnt/user/appdata2/x"),
        ("/mnt/user/liveness", "/mnt/user/liveness"),
        // Longest prefix wins.
        ("/mnt/user/live/deep/a", "/mnt/disk2/deep/a"),
        ("/mnt/user/live/deeper", "/mnt/disk1/live/deeper"),
        // Unmapped share, the other spelling and relative paths pass through.
        ("/mnt/user/system/x", "/mnt/user/system/x"),
        ("/mnt/disk1/appdata/x", "/mnt/disk1/appdata/x"),
        ("relative/mnt/user/appdata", "relative/mnt/user/appdata"),
        // Paths that do not exist are mapped too (pure string work).
        (
            "/mnt/user/live/projects/nowhere/at/all",
            "/mnt/disk1/live/projects/nowhere/at/all",
        ),
    ] {
        assert_eq!(a.logical(Path::new(input)), PathBuf::from(want), "{input}");
    }
    assert_eq!(
        a.physical_old(Path::new("/mnt/disk1/live/p")),
        PathBuf::from("/mnt/user/live/p")
    );
}

#[test]
fn non_utf8_paths_pass_through_and_map() {
    use std::os::unix::ffi::OsStrExt;
    let a = alias();
    let odd = Path::new(std::ffi::OsStr::from_bytes(b"/mnt/user/live/\xff\xfe"));
    let mapped = a.logical(odd);
    assert_eq!(
        mapped.as_os_str().as_bytes(),
        b"/mnt/disk1/live/\xff\xfe".as_slice()
    );
    let other = Path::new(std::ffi::OsStr::from_bytes(b"/elsewhere/\xff"));
    assert_eq!(a.logical(other), other);
}

#[test]
fn process_alias_is_inert_until_installed() {
    let t = tempfile::tempdir().unwrap();
    let from = t.path().join("user/appdata");
    let to = t.path().join("disk/appdata");
    let x = from.join("x");
    assert_eq!(logical(&x), x);
    assert!(!same_path(&x, &to.join("x")));
    let entry = Entry {
        from: from.clone(),
        to: to.clone(),
        proof: "test".into(),
    };
    install(std::slice::from_ref(&entry));
    assert_eq!(logical(&x), to.join("x"));
    assert!(same_path(&x, &to.join("x")));
    assert!(starts_with(&x, &to));
    assert_eq!(
        logical_str(&x.to_string_lossy()),
        to.join("x").to_string_lossy()
    );
    uninstall(&[entry]);
    assert_eq!(logical(&x), x);
}
