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
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
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
                "Unraid appdata is unavailable; start the array before Codex Switcher"
            );
        }
        ensure!(root.is_absolute(), "data directory must be absolute");
        private_dir(&root)?;
        let root = fs::canonicalize(root)?;
        let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        let runtime = PathBuf::from(format!(
            "/tmp/codex-switcher-{}-{}",
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

#[cfg(test)]
mod tests {

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
