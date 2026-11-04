//! Free-space preflight and ENOSPC mapping (contract 1.41.0).
//!
//! Multi-step mutating operations (archive, restore, delete, switch, restart, resume, launch,
//! relocate, move, migrate) refuse up front when the filesystem holding the data root has less
//! than `min_free_disk_mb` free, instead of failing half way and leaving a stopped session.
//! A write that still hits ENOSPC is reported as `DISK_FULL`, not a raw I/O message.

use crate::paths::Paths;
use anyhow::{Result, anyhow};
use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

/// Default of `min_free_disk_mb`.
pub const DEFAULT_MIN_FREE_MB: u64 = 512;

/// The structured form of an `INSUFFICIENT_DISK_SPACE` refusal.
#[derive(Debug)]
pub struct InsufficientSpace {
    pub operation: String,
    pub path: String,
    pub free_bytes: u64,
    pub needed_bytes: u64,
}
impl std::fmt::Display for InsufficientSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "INSUFFICIENT_DISK_SPACE: {} needs at least {} MB free on the filesystem holding {} but only {} MB is free ({} of {} bytes); free space or lower config min_free_disk_mb. Nothing was changed.",
            self.operation,
            self.needed_bytes / MB,
            self.path,
            self.free_bytes / MB,
            self.free_bytes,
            self.needed_bytes
        )
    }
}
impl std::error::Error for InsufficientSpace {}
const MB: u64 = 1024 * 1024;

/// Bytes available to an unprivileged writer on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// The configured floor in bytes (0 disables the preflight).
fn floor_bytes(paths: &Paths) -> u64 {
    crate::store::load_config(paths)
        .map(|c| c.min_free_disk_mb)
        .unwrap_or(DEFAULT_MIN_FREE_MB)
        .saturating_mul(MB)
}

/// Refuse `operation` with `INSUFFICIENT_DISK_SPACE` when the root filesystem is below the floor.
/// An unreadable free-space figure never blocks anything.
pub fn preflight(paths: &Paths, operation: &str) -> Result<()> {
    check(&paths.root, floor_bytes(paths), operation, free_bytes)
}

fn check(
    root: &Path,
    needed: u64,
    operation: &str,
    free: impl Fn(&Path) -> Option<u64>,
) -> Result<()> {
    if needed == 0 {
        return Ok(());
    }
    // The root may not exist yet; its closest existing parent is on the same filesystem.
    let mut probe = root;
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p,
            None => return Ok(()),
        }
    }
    match free(probe) {
        Some(free_bytes) if free_bytes < needed => Err(anyhow!(InsufficientSpace {
            operation: operation.to_owned(),
            path: root.display().to_string(),
            free_bytes,
            needed_bytes: needed,
        })),
        _ => Ok(()),
    }
}

/// `api disk-space`: what the preflight sees right now.
pub fn report(paths: &Paths) -> serde_json::Value {
    let needed = floor_bytes(paths);
    let free = free_bytes(&paths.root);
    serde_json::json!({"since_contract":"1.41.0","path":paths.root,"free_bytes":free,
        "min_free_bytes":needed,"min_free_disk_mb":needed / MB,
        "ok":free.is_none_or(|f| f >= needed),
        "operations":["archive","restore","delete","switch","restart","resume","relocate","move","launch","account delete","shared migrate","layout migrate"],
        "refusal":"INSUFFICIENT_DISK_SPACE","write_failure":"DISK_FULL"})
}

/// Whether an error (or any cause in its chain) is "no space left on device" (ENOSPC / EDQUOT).
/// Errors that crossed a process boundary as text are recognized by the OS message.
pub fn is_disk_full(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(io_disk_full)
            || cause.to_string().contains("No space left on device")
    })
}
fn io_disk_full(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT))
        || e.kind() == std::io::ErrorKind::StorageFull
}

/// The public error text for a disk-full failure; the original message follows.
pub fn disk_full_message(error: &anyhow::Error) -> String {
    format!(
        "DISK_FULL: no space left on the device holding the data root ({error:#}); free space and retry. Cached reads (`api state --cached`) still work."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_below_the_floor_with_both_figures() {
        let dir = tempfile::tempdir().unwrap();
        let err = check(dir.path(), 300 * MB, "archive", |_| Some(100 * MB)).unwrap_err();
        let s = err.downcast_ref::<InsufficientSpace>().unwrap();
        assert_eq!((s.free_bytes, s.needed_bytes), (100 * MB, 300 * MB));
        assert!(
            err.to_string()
                .starts_with("INSUFFICIENT_DISK_SPACE: archive")
        );
        assert!(check(dir.path(), 300 * MB, "archive", |_| Some(300 * MB)).is_ok());
        // Disabled, or an unreadable figure, never blocks.
        assert!(check(dir.path(), 0, "archive", |_| Some(0)).is_ok());
        assert!(check(dir.path(), 300 * MB, "archive", |_| None).is_ok());
    }

    #[test]
    fn a_missing_root_is_measured_through_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("not/yet/there");
        let err = check(&root, 10 * MB, "launch", |p| {
            assert_eq!(p, dir.path());
            Some(MB)
        })
        .unwrap_err();
        assert!(err.downcast_ref::<InsufficientSpace>().is_some());
    }

    #[test]
    fn enospc_is_found_anywhere_in_the_chain() {
        let raw = std::io::Error::from_raw_os_error(libc::ENOSPC);
        let wrapped = anyhow::Error::new(raw)
            .context("write snapshot")
            .context("state");
        assert!(is_disk_full(&wrapped));
        assert!(!is_disk_full(&anyhow!("other")));
        let quota = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EDQUOT));
        assert!(is_disk_full(&quota));
        assert!(!is_disk_full(&anyhow::Error::new(
            std::io::Error::from_raw_os_error(libc::EACCES)
        )));
    }

    #[test]
    fn the_real_filesystem_reports_a_figure() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).is_some());
    }
}
