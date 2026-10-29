use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub runtime: PathBuf,
}
impl Paths {
    /// Resolve and, if missing, create a data root. Only `setup` (and disposable
    /// test fixtures) may create a root; every other command uses [`Paths::open`].
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
        Self::resolve(root, true)
    }
    /// Resolve an existing data root and fail closed when it is absent, so a
    /// stale `SESSIONSATION_HOME` or an unmounted array never creates a fresh,
    /// empty shadow root.
    pub fn open(root: Option<PathBuf>) -> Result<Self> {
        Self::resolve(root, false)
    }
    fn resolve(root: Option<PathBuf>, create: bool) -> Result<Self> {
        let unraid = Path::new("/etc/unraid-version").is_file();
        let root = data_root(
            root,
            std::env::var_os("SESSIONSATION_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
            unraid,
        )?;
        if unraid && root.starts_with("/mnt/user/") {
            ensure!(
                Path::new("/mnt/user/appdata").is_dir(),
                "Unraid appdata is unavailable; start the array before Sessionsation"
            );
        }
        ensure!(root.is_absolute(), "data directory must be absolute");
        if !create
            && fs::symlink_metadata(&root).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            anyhow::bail!(
                "DATA_ROOT_MISSING: data root {} does not exist; run `sessionsation setup` to create a new one",
                root.display()
            );
        }
        private_dir(&root)?;
        let root = fs::canonicalize(root)?;
        let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        let runtime = PathBuf::from(format!(
            "/tmp/sessionsation-{}-{}",
            unsafe { libc::geteuid() },
            &hash[..16]
        ));
        private_dir(&runtime)?;
        Ok(Self { root, runtime })
    }
    pub fn control(&self) -> PathBuf {
        self.runtime.join("control.sock")
    }
    pub fn account(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join("accounts").join(name))
    }
    pub fn session(&self, id: &str) -> Result<PathBuf> {
        validate_name(id)?;
        Ok(self.root.join("sessions").join(id))
    }
}
// Resolve overrides before HOME so explicit and Unraid paths also work at boot.
fn data_root(
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    home: Option<PathBuf>,
    unraid: bool,
) -> Result<PathBuf> {
    if let Some(root) = explicit.or(environment) {
        return Ok(root);
    }
    if unraid {
        return Ok(PathBuf::from("/mnt/user/appdata/sessionsation"));
    }
    Ok(home
        .context("HOME is unset; pass --data-dir or set SESSIONSATION_HOME")?
        .join(".local/share/sessionsation"))
}

pub const NEW_ACCOUNT_NAME_MAX: usize = 30;
pub fn validate_new_account_name(name: &str) -> Result<()> {
    validate_name(name)?;
    ensure!(name.len() <= NEW_ACCOUNT_NAME_MAX, "ACCOUNT_NAME_TOO_LONG");
    Ok(())
}
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "name must contain 1–64 ASCII letters, digits, hyphens, or underscores"
    );
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    if let Ok(m) = fs::symlink_metadata(path) {
        ensure!(
            m.is_dir() && !m.file_type().is_symlink() && m.uid() == unsafe { libc::geteuid() },
            "unsafe directory {}",
            path.display()
        );
    }
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    private_dir(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn lock_file(path: &Path) -> Result<fs::File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}

/// Atomic replacement can hide a changing JSON directory entry from a concurrent
/// readdir (notably on overlayfs). Worker locks/logs have stable entries: resolve
/// their corresponding journal directly instead of interpreting omission as absence.
pub(crate) fn journal_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let entries = fs::read_dir(directory)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    Ok(journal_paths_from_entries(directory, entries))
}
fn journal_paths_from_entries(directory: &Path, entries: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut paths = std::collections::BTreeSet::new();
    for path in entries {
        if path.extension().is_some_and(|s| s == "json") {
            paths.insert(path.clone());
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        for suffix in [".worker-lock", ".worker.lock", ".log", ".lock"] {
            if let Some(stem) = name.strip_suffix(suffix) {
                let journal = directory.join(format!("{stem}.json"));
                if !stem.is_empty() && journal.is_file() {
                    paths.insert(journal);
                }
                break;
            }
        }
    }
    paths.into_iter().collect()
}

/// Directories under the data root that only sessionsation writes through
/// [`atomic_write`]. Account homes, archives, backups and project data are
/// deliberately absent.
const TEMP_SWEEP_DIRS: &[&str] = &[
    "messaging",
    "observations",
    "conversations",
    "deletions",
    "session-catalog",
    "launches",
    "conversation-operations",
    "account-deletions",
    "account-catalog",
    "claude-steer",
    "claude-deliveries",
    "handoff-jobs",
    "message-recoveries",
    "resumptions",
    "thread-locations",
    "managed-panes",
    "herdr-panes",
    "lifecycle-admission",
];
pub const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// `tempfile::NamedTempFile` names: exactly `.tmp` plus six alphanumerics.
fn is_temp_name(name: &str) -> bool {
    name.strip_prefix(".tmp")
        .is_some_and(|r| r.len() == 6 && r.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Remove temp files that an interrupted [`atomic_write`] left behind: regular
/// files (never symlinks) owned by this user, named exactly like the helper's
/// temp files and untouched for `max_age`. Returns how many were removed.
pub fn sweep_stale_temps(root: &Path, max_age: std::time::Duration) -> usize {
    fn walk(dir: &Path, depth: usize, max_age: std::time::Duration, uid: u32, n: &mut usize) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < 6 {
                    walk(&path, depth + 1, max_age, uid, n);
                }
                continue;
            }
            let name = entry.file_name();
            if !kind.is_file() || !name.to_str().is_some_and(is_temp_name) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let old = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age >= max_age);
            if meta.file_type().is_file()
                && meta.uid() == uid
                && old
                && fs::remove_file(&path).is_ok()
            {
                *n += 1;
            }
        }
    }
    let uid = unsafe { libc::geteuid() };
    let mut removed = 0;
    for name in TEMP_SWEEP_DIRS {
        let dir = root.join(name);
        if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir()) {
            walk(&dir, 0, max_age, uid, &mut removed);
        }
    }
    removed
}

#[cfg(test)]
mod tests {

    #[test]
    fn stale_atomic_write_temps_are_swept_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("observations/native-turns");
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join(".tmpl2no68");
        let fresh = dir.join(".tmpAbC123");
        let lookalike = dir.join(".tmpl2no68.json");
        let real = dir.join("turn.json");
        let link_target = root.path().join("elsewhere");
        std::fs::write(&link_target, "x").unwrap();
        let link = dir.join(".tmpZZZZZZ");
        std::os::unix::fs::symlink(&link_target, &link).unwrap();
        let outside = root.path().join("claude-accounts/a");
        std::fs::create_dir_all(&outside).unwrap();
        let outside = outside.join(".tmpl2no68");
        for p in [&stale, &fresh, &lookalike, &real, &outside] {
            std::fs::write(p, "x").unwrap();
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        for p in [&stale, &lookalike, &real, &outside] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        assert_eq!(
            super::sweep_stale_temps(root.path(), super::STALE_TEMP_AGE),
            1
        );
        assert!(!stale.exists());
        assert!(fresh.exists(), "recent temp may belong to a live writer");
        assert!(lookalike.exists() && real.exists());
        assert!(std::fs::symlink_metadata(&link).is_ok() && link_target.exists());
        assert!(outside.exists(), "account homes are not swept");
    }
    #[test]
    fn stable_worker_entries_recover_journals_omitted_by_readdir() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let mut entries = vec![];
        for (id, suffix) in [
            ("move", ".worker-lock"),
            ("resume", ".worker.lock"),
            ("legacy", ".log"),
            ("space", ".lock"),
        ] {
            let journal = dir.join(format!("{id}.json"));
            std::fs::write(&journal, "{}").unwrap();
            let anchor = dir.join(format!("{id}{suffix}"));
            std::fs::write(&anchor, "").unwrap();
            entries.push(anchor);
        }
        entries.push(dir.join("queue.lock"));
        entries.push(dir.join("orphan.worker.lock"));
        entries.push(dir.join(".tmp-unpublished"));
        entries.push(dir.join("move.json"));
        let found = super::journal_paths_from_entries(dir, entries);
        assert_eq!(found.len(), 4);
        assert!(found.iter().all(|p| p.extension().unwrap() == "json"));
    }
    use super::data_root;
    use std::path::PathBuf;

    #[test]
    fn new_account_limit_does_not_reject_readable_legacy_accounts() {
        super::validate_new_account_name(&"x".repeat(30)).unwrap();
        assert!(super::validate_new_account_name(&"x".repeat(31)).is_err());
        assert!(super::validate_new_account_name("non ascii 界").is_err());
        super::validate_name(&"x".repeat(64)).unwrap();
    }

    #[test]
    fn only_explicit_creation_makes_a_missing_root() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("missing");
        let error = super::Paths::open(Some(root.clone()))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("DATA_ROOT_MISSING"), "{error}");
        assert!(!root.exists());
        let created = super::Paths::new(Some(root.clone())).unwrap();
        assert!(
            created
                .runtime
                .to_string_lossy()
                .starts_with("/tmp/sessionsation-")
        );
        let opened = super::Paths::open(Some(root.clone())).unwrap();
        assert_eq!(opened.root, created.root);
        assert_eq!(opened.runtime, created.runtime);
        std::os::unix::fs::symlink(&root, t.path().join("alias")).unwrap();
        assert!(super::Paths::open(Some(t.path().join("alias"))).is_err());
    }
    #[test]
    fn unraid_defaults_to_persistent_appdata_without_home() {
        assert_eq!(
            data_root(None, None, None, true).unwrap(),
            PathBuf::from("/mnt/user/appdata/sessionsation")
        );
    }

    #[test]
    fn explicit_path_and_environment_override_platform_defaults() {
        for unraid in [false, true] {
            assert_eq!(
                data_root(
                    Some("/explicit".into()),
                    Some("/environment".into()),
                    None,
                    unraid
                )
                .unwrap(),
                PathBuf::from("/explicit")
            );
            assert_eq!(
                data_root(None, Some("/environment".into()), None, unraid).unwrap(),
                PathBuf::from("/environment")
            );
        }
    }

    #[test]
    fn other_hosts_keep_the_home_default() {
        assert_eq!(
            data_root(None, None, Some("/home/alice".into()), false).unwrap(),
            PathBuf::from("/home/alice/.local/share/sessionsation")
        );
        assert!(data_root(None, None, None, false).is_err());
    }
}
