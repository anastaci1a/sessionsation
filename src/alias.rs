//! Root alias (contract 1.54.0): two spellings of one directory tree.
//!
//! On Unraid `/mnt/user/appdata` is a bind mount of `/mnt/disk1/appdata`: both
//! spellings name the same inodes, but `fs::canonicalize` returns each spelling
//! unchanged (a bind is not a symlink). Once such a pair has been proven and
//! activated (layout version 3, `layout.json` `root_alias`), every comparison of
//! two paths (homes, working directories, roots, transcripts) compares their
//! [`logical`] forms: the longest matching `from` prefix (on component
//! boundaries) is replaced by its `to`. Stored values keep the spelling in which
//! they were recorded; only comparisons and new requests are normalised.
//!
//! The rule is enforced by a source gate (`tests::gate::no_raw_path_comparisons`):
//! `fs::canonicalize(` and `==`/`!=` on home/cwd/directory/root values outside
//! this module must either go through [`canonical`], [`same_path`],
//! [`logical`] or `layout::same_home`, or carry a `raw-path:` comment saying
//! why the literal spelling is wanted.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::RwLock,
};

/// One proven prefix pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub from: PathBuf,
    pub to: PathBuf,
    /// `same_inode` | `probe_file` (how the pair was proven when activated).
    #[serde(default)]
    pub proof: String,
}

/// A set of prefix pairs. Pure string work: it also maps paths that do not exist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Alias {
    pub entries: Vec<Entry>,
}
impl Alias {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self { entries }
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// The entry whose `from` is the longest prefix of `path` (component-wise).
    pub fn entry_for(&self, path: &Path) -> Option<&Entry> {
        self.entries
            .iter()
            .filter(|e| path.starts_with(&e.from))
            .max_by_key(|e| e.from.components().count())
    }
    /// `path` with its longest matching `from` prefix replaced by `to`.
    pub fn logical(&self, path: &Path) -> PathBuf {
        match self.entry_for(path) {
            Some(e) => match path.strip_prefix(&e.from) {
                Ok(rest) if rest.as_os_str().is_empty() => e.to.clone(),
                Ok(rest) => e.to.join(rest),
                Err(_) => path.to_path_buf(),
            },
            None => path.to_path_buf(),
        }
    }
    /// The inverse mapping (`to` prefix back to `from`), for rollback and reports.
    pub fn physical_old(&self, path: &Path) -> PathBuf {
        let best = self
            .entries
            .iter()
            .filter(|e| path.starts_with(&e.to))
            .max_by_key(|e| e.to.components().count());
        match best.and_then(|e| path.strip_prefix(&e.to).ok().map(|r| (e, r))) {
            Some((e, rest)) if rest.as_os_str().is_empty() => e.from.clone(),
            Some((e, rest)) => e.from.join(rest),
            None => path.to_path_buf(),
        }
    }
    pub fn maps(&self, path: &Path) -> bool {
        self.entry_for(path).is_some()
    }
}

/// Every alias entry activated by a root this process opened. A process opens
/// one root in production; test processes open many temporary roots whose
/// prefixes never overlap, so a union is safe.
static ACTIVE: RwLock<Vec<Entry>> = RwLock::new(Vec::new());
/// Requested spelling of each opened root (canonical root -> as requested).
static REQUESTED: RwLock<BTreeMap<PathBuf, PathBuf>> = RwLock::new(BTreeMap::new());

/// Activate `entries` for this process (idempotent).
pub fn install(entries: &[Entry]) {
    let mut active = ACTIVE.write().unwrap_or_else(|e| e.into_inner());
    for e in entries {
        if !active.iter().any(|a| a.from == e.from && a.to == e.to) {
            active.push(e.clone());
        }
    }
}
/// Deactivate `entries`.
pub fn uninstall(entries: &[Entry]) {
    let mut active = ACTIVE.write().unwrap_or_else(|e| e.into_inner());
    active.retain(|a| !entries.iter().any(|e| a.from == e.from && a.to == e.to));
}
/// The process-wide alias.
pub fn current() -> Alias {
    Alias::new(ACTIVE.read().unwrap_or_else(|e| e.into_inner()).clone())
}
pub fn active() -> bool {
    !ACTIVE.read().unwrap_or_else(|e| e.into_inner()).is_empty()
}
pub(crate) fn record_requested(root: &Path, requested: &Path) {
    REQUESTED
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(root.to_path_buf(), requested.to_path_buf());
}
/// The root spelling a caller asked for (`--data-dir`, `SESSIONSATION_HOME` or the default).
pub fn requested_root(root: &Path) -> PathBuf {
    REQUESTED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(root)
        .cloned()
        .unwrap_or_else(|| root.to_path_buf())
}

/// [`Alias::logical`] with the process-wide alias.
pub fn logical(path: &Path) -> PathBuf {
    let active = ACTIVE.read().unwrap_or_else(|e| e.into_inner());
    if active.is_empty() {
        return path.to_path_buf();
    }
    Alias::new(active.clone()).logical(path)
}
/// [`logical`] for a string value; non-absolute text is returned unchanged.
pub fn logical_str(text: &str) -> String {
    let path = Path::new(text);
    if !path.is_absolute() {
        return text.to_owned();
    }
    logical(path).to_string_lossy().into_owned()
}
/// [`logical_str`] for each space-separated absolute word of a command line
/// (for example the built-in statusLine command `bash /mnt/user/...`).
pub fn logical_words(text: &str) -> String {
    if !active() {
        return text.to_owned();
    }
    text.split(' ')
        .map(logical_str)
        .collect::<Vec<_>>()
        .join(" ")
}
/// `fs::canonicalize` followed by [`logical`]: the comparison form of an existing path.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(|p| logical(&p)) // raw-path: the one alias-aware wrapper
}
/// Two paths name the same location: equal as given, or equal in logical form.
pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || logical(a) == logical(b) // raw-path: defines the comparison
}
/// [`same_path`] for string values.
pub fn same_str(a: &str, b: &str) -> bool {
    same_path(Path::new(a), Path::new(b))
}
/// `path` relative to `prefix`, as given or in logical form (either spelling of each).
pub fn strip_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    if let Ok(rest) = path.strip_prefix(prefix) {
        return Some(rest.to_path_buf());
    }
    logical(path)
        .strip_prefix(logical(prefix))
        .ok()
        .map(Path::to_path_buf)
}
/// `path` starts with `prefix` in logical form.
pub fn starts_with(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix) || logical(path).starts_with(logical(prefix))
}

/// The share spelling and its disk spelling (Unraid FUSE share vs the array disk).
pub const SHARE_SPELLING: (&str, &str) = ("/mnt/user", "/mnt/disk1");
/// A search-path list (`PATH`) with each `<share>/<rest>` entry rewritten to `<disk>/<rest>`
/// when that directory exists and is the same directory (same device and inode), so a tool is
/// never resolved through the slow FUSE share. Order is kept; later duplicates are dropped.
/// Entries that are not under `share`, or whose disk spelling is missing or different, stay.
pub fn normalize_search_path(
    list: &std::ffi::OsStr,
    share: &Path,
    disk: &Path,
) -> std::ffi::OsString {
    use std::os::unix::fs::MetadataExt;
    let mut seen: Vec<PathBuf> = Vec::new();
    for entry in std::env::split_paths(list) {
        let mapped = entry
            .strip_prefix(share)
            .ok()
            .map(|rest| disk.join(rest))
            .filter(
                |candidate| match (std::fs::metadata(&entry), std::fs::metadata(candidate)) {
                    (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                    _ => false,
                },
            )
            .unwrap_or(entry);
        if !seen.contains(&mapped) {
            seen.push(mapped);
        }
    }
    std::env::join_paths(seen).unwrap_or_else(|_| list.to_owned())
}
/// Normalize this process's own `PATH` ([`normalize_search_path`] with [`SHARE_SPELLING`]).
/// Call before any thread starts: children (providers, workers) inherit it.
pub fn normalize_process_path() {
    if let Some(list) = std::env::var_os("PATH") {
        let (share, disk) = SHARE_SPELLING;
        let fixed = normalize_search_path(&list, Path::new(share), Path::new(disk));
        if fixed != list {
            // SAFETY: called first thing in `main`, before the runtime or any thread exists.
            unsafe { std::env::set_var("PATH", fixed) };
        }
    }
}

pub mod runtime;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod unit {
    use super::*;

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
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;

    #[test]
    fn a_share_path_entry_becomes_the_disk_entry_in_order_without_duplicates() {
        let t = tempfile::tempdir().unwrap();
        let disk = t.path().join("disk1");
        let share = t.path().join("user");
        for d in ["live/packages/a/bin", "live/packages/b/bin", "other/bin"] {
            fs::create_dir_all(disk.join(d)).unwrap();
        }
        // The share is the same directory tree (a symlink stands in for the FUSE view).
        std::os::unix::fs::symlink(&disk, &share).unwrap();
        // A second tree under the share that is NOT the disk one.
        let lone = t.path().join("lone");
        fs::create_dir_all(lone.join("x/bin")).unwrap();
        let join = |parts: Vec<PathBuf>| std::env::join_paths(parts).unwrap();
        let list = join(vec![
            "/usr/local/bin".into(),
            share.join("live/packages/a/bin"),
            disk.join("live/packages/a/bin"), // duplicate after rewriting
            share.join("live/packages/b/bin"),
            share.join("missing/bin"), // no disk twin: stays
            "/usr/bin".into(),
            "/usr/local/bin".into(), // plain duplicate
            share.join("live/packages/b/bin"),
        ]);
        let got = normalize_search_path(&list, &share, &disk);
        let want = join(vec![
            "/usr/local/bin".into(),
            disk.join("live/packages/a/bin"),
            disk.join("live/packages/b/bin"),
            share.join("missing/bin"),
            "/usr/bin".into(),
        ]);
        assert_eq!(got, want);
        // Different directories with the same relative path are not rewritten.
        let other_share = t.path().join("share2");
        fs::create_dir_all(other_share.join("other/bin")).unwrap();
        let list: OsString = join(vec![other_share.join("other/bin")]);
        assert_eq!(normalize_search_path(&list, &other_share, &disk), list);
    }
}
