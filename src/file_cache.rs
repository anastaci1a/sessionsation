//! In-process cache of parsed JSON files, keyed on the file's identity and version.
//!
//! The supervisor's periodic passes re-read the same journal records and registry files
//! every few seconds. A cached entry is valid while the file's `(device, inode, size,
//! mtime, ctime)` stamp is unchanged; every lookup still `stat`s the file, so a record
//! replaced by [`crate::paths::atomic_write`] (a new inode) or rewritten in place is seen
//! at once. A file modified within [`RACY_SECONDS`] of the lookup is never cached: an
//! in-place rewrite that keeps size and timestamp granularity cannot hide behind it.
//! The cache holds parsed values only; callers keep their own locking and writing paths.
use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// Files younger than this many seconds are parsed on every lookup.
pub const RACY_SECONDS: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Stamp {
    pub fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
    /// Old enough that a later in-place rewrite would still change the stamp.
    pub fn settled(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let window = racy_seconds();
        now - self.mtime.0 >= window && now - self.ctime.0 >= window
    }
}

fn racy_seconds() -> i64 {
    #[cfg(test)]
    let seconds = tests::racy_seconds(RACY_SECONDS);
    #[cfg(not(test))]
    let seconds = RACY_SECONDS;
    seconds
}

/// Why a cached load failed: reading the file, or parsing what was read.
#[derive(Debug)]
pub enum LoadError<E> {
    Io(io::Error),
    Parse(E),
}

pub struct FileCache<T> {
    entries: Mutex<BTreeMap<PathBuf, (Stamp, Arc<T>)>>,
}
impl<T> Default for FileCache<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T> FileCache<T> {
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }
    /// The parsed file. `stat` always runs first; the bytes are read and parsed only when the
    /// stamp differs from the cached one (a stamp taken before the read is conservative: a
    /// file that changes in between is simply loaded again next time).
    pub fn load<E>(
        &self,
        path: &Path,
        parse: impl FnOnce(&[u8]) -> Result<T, E>,
    ) -> Result<Arc<T>, LoadError<E>> {
        let stamp = Stamp::of(&fs::metadata(path).map_err(LoadError::Io)?);
        if let Ok(entries) = self.entries.lock()
            && let Some((cached, value)) = entries.get(path)
            && *cached == stamp
        {
            return Ok(value.clone());
        }
        let value =
            Arc::new(parse(&fs::read(path).map_err(LoadError::Io)?).map_err(LoadError::Parse)?);
        if let Ok(mut entries) = self.entries.lock() {
            if stamp.settled() {
                entries.insert(path.to_owned(), (stamp, value.clone()));
            } else {
                entries.remove(path);
            }
        }
        Ok(value)
    }
    /// Forget entries directly inside `dir` that `keep` rejects (files that left the directory).
    pub fn retain_dir(&self, dir: &Path, keep: &std::collections::HashSet<&Path>) {
        if let Ok(mut entries) = self.entries.lock() {
            // raw-path: keys are the exact spellings the caller listed and loaded
            entries.retain(|path, _| path.parent() != Some(dir) || keep.contains(path.as_path()));
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::trust_fresh_stamps;
