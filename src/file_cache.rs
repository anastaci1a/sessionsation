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
    if let Some(window) = TEST_WINDOW.with(|w| w.get()) {
        return window;
    }
    RACY_SECONDS
}
#[cfg(test)]
thread_local! {
    static TEST_WINDOW: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}
/// Tests cannot back-date a file's ctime: let this thread's cache trust every stamp
/// (`true`), or restore the real window (`false`).
#[cfg(test)]
pub(crate) fn trust_fresh_stamps(on: bool) {
    TEST_WINDOW.with(|w| w.set(on.then_some(i64::MIN / 2)));
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
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn parse_counting<'a>(n: &'a AtomicUsize) -> impl FnOnce(&[u8]) -> Result<String, ()> + 'a {
        move |b| {
            n.fetch_add(1, Ordering::SeqCst);
            Ok(String::from_utf8_lossy(b).into_owned())
        }
    }

    #[test]
    fn a_settled_file_is_parsed_once_and_a_rewrite_is_seen() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.json");
        let c = FileCache::<String>::new();
        let n = AtomicUsize::new(0);
        trust_fresh_stamps(true);
        fs::write(&p, "one").unwrap();
        for _ in 0..3 {
            assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "one");
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
        // Rewritten in place with a different size.
        fs::write(&p, "two!").unwrap();
        assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "two!");
        assert_eq!(n.load(Ordering::SeqCst), 2);
        // Replaced by rename (new inode).
        let tmp = d.path().join("tmp");
        fs::write(&tmp, "333").unwrap();
        fs::rename(&tmp, &p).unwrap();
        assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "333");
        assert_eq!(n.load(Ordering::SeqCst), 3);
        // And served from the cache again afterwards.
        assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "333");
        assert_eq!(n.load(Ordering::SeqCst), 3);
        trust_fresh_stamps(false);
    }

    #[test]
    fn a_just_written_file_is_never_served_from_the_cache() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.json");
        let c = FileCache::<String>::new();
        let n = AtomicUsize::new(0);
        fs::write(&p, "one").unwrap();
        assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "one");
        fs::write(&p, "two").unwrap(); // same size, same second
        assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "two");
        assert_eq!(n.load(Ordering::SeqCst), 2);
        assert!(c.is_empty());
    }

    #[test]
    fn errors_are_not_cached_and_removed_files_leave_the_cache() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.json");
        let c = FileCache::<String>::new();
        assert!(matches!(
            c.load(&p, |_| Ok::<_, ()>(String::new())),
            Err(LoadError::Io(_))
        ));
        trust_fresh_stamps(true);
        fs::write(&p, "x").unwrap();
        assert!(matches!(
            c.load(&p, |_| Err::<String, _>(())),
            Err(LoadError::Parse(()))
        ));
        assert!(c.load(&p, |_| Ok::<_, ()>("x".into())).is_ok());
        assert_eq!(c.len(), 1);
        c.retain_dir(d.path(), &std::collections::HashSet::new());
        assert!(c.is_empty());
        trust_fresh_stamps(false);
    }
}
