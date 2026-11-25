//! Compatibility runtime paths (review 2, B2).
//!
//! The runtime directory (control socket, hosted sockets) is
//! `/tmp/sessionsation-<uid>-<hash(root spelling)>`. Clients that compute it from
//! their configured root string, notably Agent-DM (`sessionOwnerSocket`: sha256
//! of `realpathSync(dataRoot)`, which does not resolve a bind), keep the old
//! spelling's path after activation. While the alias is active, every old
//! spelling of the root gets a directory symlink from its runtime path to the
//! canonical runtime directory. `lstat("<old>/control.sock")` then reports the
//! real socket (only the final component is not followed), so such a client
//! keeps working until it is repointed.
//!
//! Safety: a symlink is created only where nothing exists, or replaces nothing.
//! A real directory is moved aside (never removed) only when it is ours, private
//! and holds no connectable socket; a live socket is never touched. Rollback
//! removes only symlinks that point at the canonical runtime directory.
use super::Alias;
use crate::{
    layout::{self, RootAlias},
    paths::Paths,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
};

/// Old spellings whose runtime path should reach the canonical runtime.
fn old_spellings(record: &RootAlias, canonical: &Path) -> Vec<PathBuf> {
    let a = Alias::new(record.entries.clone());
    let mut out = vec![];
    for p in [record.previous_root.clone(), a.physical_old(canonical)] {
        // raw-path: literal spellings name distinct runtime paths
        if !p.as_os_str().is_empty() && p != canonical && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}
fn live_socket_in(dir: &Path) -> bool {
    fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        e.file_type().is_ok_and(|t| {
            use std::os::unix::fs::FileTypeExt;
            t.is_socket()
        }) && std::os::unix::net::UnixStream::connect(e.path()).is_ok()
    })
}

/// Ensure the compatibility links for an active alias. Returns one line per
/// spelling (created, present, moved_aside, refused: reason).
pub fn ensure(paths: &Paths) -> Vec<Value> {
    let Ok(Some(record)) = layout::read_record(&paths.root) else {
        return vec![];
    };
    let Some(a) = record.root_alias.filter(RootAlias::is_active) else {
        return vec![];
    };
    // The canonical runtime (the process may still have opened the old spelling,
    // as Tier R does before activation).
    let runtime = crate::paths::runtime_for(&a.canonical_root);
    if crate::paths::private_dir(&runtime).is_err() {
        return vec![
            json!({"path":runtime,"result":"refused: the canonical runtime directory is unsafe"}),
        ];
    }
    let mut out = vec![];
    for old in old_spellings(&a, &a.canonical_root) {
        let link = crate::paths::runtime_for(&old);
        // raw-path: two literal runtime paths
        if link == runtime {
            continue;
        }
        let r = ensure_one(&link, &runtime);
        out.push(json!({"spelling":old,"path":link,"result":r}));
    }
    out
}
pub(crate) fn ensure_one(link: &Path, runtime: &Path) -> String {
    let uid = unsafe { libc::geteuid() };
    match fs::symlink_metadata(link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match symlink(runtime, link) {
            Ok(()) => "created".into(),
            Err(e) => format!("refused: {e}"),
        },
        Ok(m) if m.file_type().is_symlink() => {
            if fs::read_link(link).ok().as_deref() == Some(runtime) {
                "present".into()
            } else {
                "refused: another symlink is there; it was left unchanged".into()
            }
        }
        Ok(m) if m.is_dir() => {
            if m.uid() != uid || m.mode() & 0o077 != 0 {
                return "refused: a directory not owned privately by this user".into();
            }
            if live_socket_in(link) {
                return "refused: a live socket is in it (another supervisor?)".into();
            }
            let aside = link.with_file_name(format!(
                "{}.pre-alias-{}",
                link.file_name().unwrap_or_default().to_string_lossy(),
                layout::now()
            ));
            if let Err(e) = fs::rename(link, &aside) {
                return format!("refused: cannot move the stale directory aside: {e}");
            }
            match symlink(runtime, link) {
                Ok(()) => format!("moved_aside:{}", aside.display()),
                Err(e) => format!("refused: {e}"),
            }
        }
        Ok(_) => "refused: not a directory".into(),
        Err(e) => format!("refused: {e}"),
    }
}

/// Remove the compatibility links of `run` (rollback): only symlinks that
/// point at the canonical runtime directory.
pub fn remove(run: &crate::root_migrate::Run) -> Vec<Value> {
    let canonical_runtime = crate::paths::runtime_for(&run.canonical_root);
    let a = RootAlias {
        previous_root: run.previous_root.clone(),
        entries: run.entries.clone(),
        ..Default::default()
    };
    let mut out = vec![];
    for old in old_spellings(&a, &run.canonical_root) {
        let link = crate::paths::runtime_for(&old);
        let ours = fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink())
            && fs::read_link(&link).ok().as_deref() == Some(canonical_runtime.as_path());
        if ours {
            let r = fs::remove_file(&link); // our own compatibility symlink
            out.push(json!({"path":link,"removed":r.is_ok()}));
        }
    }
    out
}
