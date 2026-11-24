//! File locks (owner of the `locks` role; skeleton). Today only the receipts ledger
//! takes its lock through here; later steps move the other `flock` sites in and add
//! their kinds to [`LockKind`] in acquisition order.
use crate::{
    layout,
    paths::{Paths, lock_file},
};
use fs2::FileExt;
use std::{fs, io, path::PathBuf};

/// Lock kinds in the order they may be nested (outer first). A holder of a kind takes
/// only kinds after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockKind {
    /// `messaging/adapter.lock`: the receipts ledger, taken inside every other lock.
    Adapter,
}

impl LockKind {
    fn path(self, paths: &Paths) -> PathBuf {
        match self {
            Self::Adapter => paths.root.join(layout::ADAPTER_LOCK),
        }
    }
}

/// An open lock file. Each `Lock` is its own open file description, so two `Lock`s
/// in one process exclude each other exactly as two processes do. Dropping it
/// releases whatever it holds.
#[derive(Debug)]
pub struct Lock {
    file: fs::File,
    kind: LockKind,
}

impl Lock {
    /// Open (creating, mode 0600, never following a symlink) the lock file of `kind`.
    pub fn open(paths: &Paths, kind: LockKind) -> io::Result<Self> {
        Ok(Self {
            file: lock_file(&kind.path(paths)).map_err(io::Error::other)?,
            kind,
        })
    }
    pub fn kind(&self) -> LockKind {
        self.kind
    }
    /// Take it exclusively without waiting; `Ok(false)` when another holder has it.
    pub fn try_exclusive(&self) -> io::Result<bool> {
        retry_eintr(|| self.file.try_lock_exclusive())
    }
    /// Take it shared without waiting; `Ok(false)` when an exclusive holder has it.
    pub fn try_shared(&self) -> io::Result<bool> {
        retry_eintr(|| FileExt::try_lock_shared(&self.file))
    }
}

/// `Ok(false)` when the lock is held elsewhere; an interrupted call is retried.
fn retry_eintr(mut attempt: impl FnMut() -> io::Result<()>) -> io::Result<bool> {
    loop {
        match attempt() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return busy_is_false(other),
        }
    }
}

fn busy_is_false(r: io::Result<()>) -> io::Result<bool> {
    match r {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_holders_coexist_and_exclude_an_exclusive_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().to_path_buf(),
            runtime: dir.path().join("run"),
        };
        fs::create_dir_all(paths.root.join("messaging")).unwrap();
        let a = Lock::open(&paths, LockKind::Adapter).unwrap();
        let b = Lock::open(&paths, LockKind::Adapter).unwrap();
        let c = Lock::open(&paths, LockKind::Adapter).unwrap();
        assert!(a.try_shared().unwrap());
        assert!(b.try_shared().unwrap());
        assert!(!c.try_exclusive().unwrap());
        drop(a);
        drop(b);
        assert!(c.try_exclusive().unwrap());
        let d = Lock::open(&paths, LockKind::Adapter).unwrap();
        assert!(!d.try_shared().unwrap());
        assert_eq!(d.kind(), LockKind::Adapter);
    }
}
