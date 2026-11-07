//! Shared Claude per-conversation state (contract 1.39.0).
//!
//! A session must behave the same and see the same information on any account,
//! so the non-credential, per-conversation state of Claude Code lives once in
//! `<root>/shared/claude/<name>` and every managed Claude account home links to
//! it with a directory symlink `<account home>/<name>` for these names:
//! `projects` (transcripts and per-project `memory/`), `file-history`, `todos`
//! and `session-env`. Contract 1.46.0 adds the user-level Claude customisation
//! directories `agents` (custom subagents), `skills` and `commands` (slash
//! commands), so a session finds the same subagent types on every account.
//! Credentials, `settings.json` (statusLine, permissions and similar values may
//! legitimately differ per account) and the live `sessions/` registry stay per
//! account.
//!
//! Sessionsation refuses symlinks nearly everywhere. [`trusted_link`] is the one
//! helper that makes exactly these account-home links acceptable; it is called
//! only where code explicitly handles a link (launch-time repair, parent checks
//! when restoring a file, account deletion, the switch no-op and the migration).
//! Generic tree walkers (`reject_symlink`, `tree_digest`, `remove_tree`) still
//! refuse a link, so no operation can copy, digest or delete *through* one.
//!
//! The feature is dormant until the operator runs `shared enable` or the first
//! `shared migrate`: a marker file in the store turns on link creation and the
//! `SHARED_STATE_UNMIGRATED` refusal. Adopted (operator-owned) config
//! directories are never linked.
use crate::{
    paths::{Paths, atomic_write, lock_file, validate_name},
    provider::ProviderId,
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
};

pub mod migrate;
#[cfg(test)]
mod tests;

/// Directory names inside a Claude config dir that are shared, mirroring the
/// real subdirectory names Claude Code uses.
pub const NAMES: [&str; 7] = [
    "projects",
    "file-history",
    "todos",
    "session-env",
    "agents",
    "skills",
    "commands",
];
/// The per-conversation names (1.39.0). An account switch only depends on these.
pub const CORE: [&str; 4] = ["projects", "file-history", "todos", "session-env"];
/// Per-user customisation (1.46.0). Migration merges these by newest file instead
/// of reporting a same-name file as a conflict.
pub const CUSTOM: [&str; 3] = ["agents", "skills", "commands"];
/// Written by `shared enable` / `shared migrate`; its presence enables the feature.
pub const MARKER: &str = ".sessionsation-shared";
const LOCK: &str = ".lock";

/// `<root>/shared/claude`.
pub fn store(root: &Path) -> PathBuf {
    root.join("shared").join("claude")
}
/// The one allowed target of the link `name`: `<root>/shared/claude/<name>`.
pub fn target(root: &Path, name: &str) -> PathBuf {
    store(root).join(name)
}
fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
/// A real directory (never a symlink) owned by this user without group/other access.
fn private_real_dir(path: &Path) -> bool {
    private_real_dir_as(path, euid())
}
fn private_real_dir_as(path: &Path, uid: u32) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_dir() && m.uid() == uid && m.mode() & 0o077 == 0)
}

/// (data root, account name) of a managed Claude home, judged by its path shape
/// alone: `<root>/accounts/claude/<name>` or the deferred legacy
/// `<root>/claude-accounts/<name>`. Adopted config directories yield `None`.
pub fn managed_home(home: &Path) -> Option<(PathBuf, String)> {
    let name = home.file_name()?.to_str()?;
    validate_name(name).ok()?;
    let parent = home.parent()?;
    let grand = parent.parent()?;
    let root = match parent.file_name()?.to_str()? {
        "claude" if grand.file_name()? == crate::layout::ACCOUNTS => grand.parent()?,
        "claude-accounts" => grand,
        _ => return None,
    };
    let canonical = crate::layout::target_home(root, ProviderId::Claude, name);
    let legacy = crate::layout::legacy_home(root, ProviderId::Claude, name);
    (home == canonical || legacy.as_deref() == Some(home))
        .then(|| (root.to_path_buf(), name.to_owned()))
}

/// The single trust decision for account-home links. True only when `link` is a
/// symlink at exactly `<managed Claude home>/<name>` for a listed name, owned by
/// this user, whose stored target is exactly `<root>/shared/claude/<name>` and
/// which resolves (no other symlink on the way) to that directory, a real
/// private directory inside real private `shared/` and `shared/claude/`.
/// Everything else (another target, a link replaced by another link, a link
/// owned by someone else, a link outside an account home) is not trusted.
pub fn trusted_link(link: &Path) -> bool {
    trusted_link_as(link, euid())
}
/// [`trusted_link`] for an explicit owner uid (the tests prove the owner check with it).
pub(crate) fn trusted_link_as(link: &Path, uid: u32) -> bool {
    let Some(name) = link
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| NAMES.contains(n))
    else {
        return false;
    };
    let Some(home) = link.parent() else {
        return false;
    };
    let Some((root, _)) = managed_home(home) else {
        return false;
    };
    let expected = target(&root, name);
    let Ok(meta) = fs::symlink_metadata(link) else {
        return false;
    };
    meta.file_type().is_symlink()
        && meta.uid() == uid
        && fs::symlink_metadata(home).is_ok_and(|m| m.file_type().is_dir())
        && fs::read_link(link).is_ok_and(|t| t == expected)
        && fs::canonicalize(link).is_ok_and(|c| c == expected)
        && private_real_dir_as(&expected, uid)
        && private_real_dir_as(&store(&root), uid)
        && private_real_dir_as(&root.join("shared"), uid)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// A trusted link to the shared store.
    Linked,
    Missing,
    /// A real, empty directory (safe to replace by a link).
    EmptyDir,
    /// A real directory holding data: needs `shared migrate`.
    Data,
    /// Any symlink that is not trusted, or a non-directory.
    Untrusted,
}
impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linked => "linked",
            Self::Missing => "missing",
            Self::EmptyDir => "empty_directory",
            Self::Data => "unmigrated",
            Self::Untrusted => "untrusted",
        }
    }
}
pub fn state_of(home: &Path, name: &str) -> LinkState {
    let path = home.join(name);
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return LinkState::Missing;
    };
    if meta.file_type().is_symlink() {
        return if trusted_link(&path) {
            LinkState::Linked
        } else {
            LinkState::Untrusted
        };
    }
    if !meta.file_type().is_dir() {
        return LinkState::Untrusted;
    }
    match fs::read_dir(&path).map(|mut d| d.next().is_none()) {
        Ok(true) => LinkState::EmptyDir,
        _ => LinkState::Data,
    }
}
/// Every per-conversation name of this home is a trusted link (what an account
/// switch depends on).
pub fn fully_linked(home: &Path) -> bool {
    CORE.iter().all(|n| state_of(home, n) == LinkState::Linked)
}
/// Every listed name, including `agents`, `skills` and `commands`, is a trusted link.
pub fn all_linked(home: &Path) -> bool {
    NAMES.iter().all(|n| state_of(home, n) == LinkState::Linked)
}
pub fn any_linked(home: &Path) -> bool {
    NAMES.iter().any(|n| trusted_link(&home.join(n)))
}

/// The feature is on for this root.
pub fn enabled(root: &Path) -> bool {
    private_real_dir(&root.join("shared"))
        && private_real_dir(&store(root))
        && fs::symlink_metadata(store(root).join(MARKER)).is_ok_and(|m| m.file_type().is_file())
}
/// Create the store (0700) and its marker. Idempotent.
pub fn enable(root: &Path) -> Result<()> {
    crate::paths::private_dir(&root.join("shared"))?;
    crate::paths::private_dir(&store(root))?;
    let marker = store(root).join(MARKER);
    if fs::symlink_metadata(&marker).is_err() {
        atomic_write(
            &marker,
            &serde_json::to_vec(&json!({"version":1,"since_contract":"1.39.0","names":NAMES}))?,
        )?;
    }
    Ok(())
}
fn ensure_store_dir(root: &Path, name: &str) -> Result<PathBuf> {
    let dir = target(root, name);
    crate::paths::private_dir(&dir)?;
    ensure!(
        private_real_dir(&dir),
        "SHARED_STATE_STORE_UNSAFE: {} must be a private directory owned by this user and not a symlink",
        dir.display()
    );
    Ok(dir)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ensure {
    /// The feature is off for this root; nothing was touched.
    Disabled,
    /// An adopted config directory; never linked.
    NotApplicable,
    /// All links are trusted; `created` lists those made by this call. `unmigrated`
    /// lists `agents`/`skills`/`commands` that still hold their own data (left
    /// untouched until `shared migrate`; they never block a launch).
    Linked {
        created: Vec<&'static str>,
        unmigrated: Vec<&'static str>,
    },
}
impl Ensure {
    pub fn public(&self) -> Value {
        match self {
            Self::Disabled => json!({"state":"disabled"}),
            Self::NotApplicable => json!({"state":"not_applicable","reason":"adopted_config_dir"}),
            Self::Linked {
                created,
                unmigrated,
            } => {
                json!({"state":"linked","created":created,"unmigrated":unmigrated})
            }
        }
    }
}

/// Create or repair the account-home links of a managed Claude home.
/// A missing name is linked, an empty real directory is replaced by a link, a
/// trusted link is kept. A real directory that holds data is never merged here:
/// it fails with `SHARED_STATE_UNMIGRATED` and the operator runs
/// `sessionsation shared migrate`. Any other symlink is refused.
pub fn ensure_links(paths: &Paths, home: &Path) -> Result<Ensure> {
    let Some((root, account)) = managed_home(home) else {
        return Ok(Ensure::NotApplicable);
    };
    if root != paths.root {
        return Ok(Ensure::NotApplicable);
    }
    if !enabled(&root) {
        return Ok(Ensure::Disabled);
    }
    ensure!(
        fs::symlink_metadata(home).is_ok_and(|m| m.file_type().is_dir()),
        "UNSAFE_ACCOUNT_HOME: {} is not a directory",
        home.display()
    );
    let lock = lock_file(&store(&root).join(LOCK))?;
    lock.lock_exclusive()?;
    let mut created = vec![];
    let mut unmigrated = vec![];
    for name in NAMES {
        let link = home.join(name);
        let shared = ensure_store_dir(&root, name)?;
        match state_of(home, name) {
            LinkState::Linked => {}
            LinkState::Missing => {
                match symlink(&shared, &link) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e).context("SHARED_STATE_LINK_FAILED"),
                }
                ensure!(
                    trusted_link(&link),
                    "SHARED_STATE_LINK_UNTRUSTED: {}",
                    link.display()
                );
                created.push(name);
            }
            LinkState::EmptyDir => {
                fs::remove_dir(&link).with_context(|| {
                    format!("SHARED_STATE_UNMIGRATED: {} is no longer empty; run `sessionsation shared migrate --account claude:{account}`", link.display())
                })?;
                symlink(&shared, &link).context("SHARED_STATE_LINK_FAILED")?;
                ensure!(
                    trusted_link(&link),
                    "SHARED_STATE_LINK_UNTRUSTED: {}",
                    link.display()
                );
                created.push(name);
            }
            LinkState::Data if CUSTOM.contains(&name) => unmigrated.push(name),
            LinkState::Data => bail!(
                "SHARED_STATE_UNMIGRATED: {} holds data that is not in the shared store; run `sessionsation shared migrate --account claude:{account} --dry-run`, then without --dry-run",
                link.display()
            ),
            LinkState::Untrusted => bail!(
                "SHARED_STATE_LINK_UNTRUSTED: {} is not a link to {}; it was left unchanged",
                link.display(),
                shared.display()
            ),
        }
    }
    Ok(Ensure::Linked {
        created,
        unmigrated,
    })
}

/// What a Claude account switch does with the conversation's files. Links of both
/// homes are ensured first. `true`: both are fully linked, so nothing is copied or
/// deleted. `false`: both are plain homes (the copy/remove move as before). A
/// half-linked pair is refused (`SHARED_STATE_MIXED`) because the plain move
/// would delete from the shared store.
pub fn switch_shared(paths: &Paths, source: &Path, target_home: &Path) -> Result<bool> {
    for home in [source, target_home] {
        ensure_links(paths, home)?;
    }
    let (a, b) = (fully_linked(source), fully_linked(target_home));
    if a && b {
        return Ok(true);
    }
    ensure!(
        !any_linked(source) && !any_linked(target_home),
        "SHARED_STATE_MIXED: only one of the two accounts uses the shared store; run `sessionsation shared migrate` for the other so a switch moves nothing instead of deleting from the shared store"
    );
    Ok(false)
}

/// Remove this home's trusted links (never anything behind them). Used by
/// account deletion so shared data survives the account. Returns the names.
pub fn detach_links(home: &Path) -> Result<Vec<&'static str>> {
    let mut removed = vec![];
    for name in NAMES {
        let link = home.join(name);
        if trusted_link(&link) {
            // unlink(2) on the symlink itself; never recursive.
            fs::remove_file(&link)
                .with_context(|| format!("SHARED_STATE_UNLINK_FAILED: {}", link.display()))?;
            removed.push(name);
        }
    }
    Ok(removed)
}

/// `renameat2(RENAME_EXCHANGE)`: atomically swap two existing paths.
pub fn rename_exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    renameat2(a, b, 2)
}
/// `renameat2(RENAME_NOREPLACE)`: move without ever overwriting.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    renameat2(from, to, 1)
}
fn renameat2(a: &Path, b: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (a, b) = (c(a)?, c(b)?);
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            flags,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Per-account link states for `shared status`.
pub fn account_status(paths: &Paths, name: &str, home: &Path) -> Value {
    match managed_home(home) {
        Some((root, _)) if root == paths.root => {
            let names: serde_json::Map<String, Value> = NAMES
                .iter()
                .map(|n| ((*n).to_owned(), json!(state_of(home, n).as_str())))
                .collect();
            json!({"account_id":format!("claude:{name}"),"home":home,"applicable":true,
                "linked":all_linked(home),"names":names})
        }
        _ => json!({"account_id":format!("claude:{name}"),"home":home,"applicable":false,
            "reason":"adopted_config_dir"}),
    }
}
/// `shared status`: every enrolled Claude account and the store itself.
pub fn status(paths: &Paths) -> Result<Value> {
    let accounts: Vec<_> = crate::provider::claude::enrolled(paths)?
        .iter()
        .map(|a| account_status(paths, &a.name, &a.home))
        .collect();
    Ok(
        json!({"enabled":enabled(&paths.root),"store":store(&paths.root),"names":NAMES,"accounts":accounts}),
    )
}
/// The capability document entry.
pub fn capability() -> Value {
    json!({"since_contract":"1.39.0","version":1,"names":NAMES,"core_names":CORE,
        "custom_names":CUSTOM,"custom_since_contract":"1.46.0",
        "custom_merge":"identical_kept_once_differing_newest_wins_other_set_aside_in_backup",
        "per_account":["settings.json","credentials","sessions"],"store":"<root>/shared/claude/<name>",
        "link":"<account home>/<name> -> <root>/shared/claude/<name>",
        "scope":"managed_claude_homes","adopted_config_dir":"never_linked",
        "enable":"shared enable | shared migrate","commands":["shared status","shared enable","shared migrate"],
        "trusted_link":"symlink at exactly <account home>/<name>, owned by this user, stored target exactly <root>/shared/claude/<name>, resolving to a real private directory",
        "account_switch":"nothing_copied_or_deleted_when_both_accounts_linked",
        "account_deletion":"removes_the_links_only",
        "errors":["SHARED_STATE_UNMIGRATED","SHARED_STATE_LINK_UNTRUSTED","SHARED_STATE_MIXED","SHARED_STATE_STORE_UNSAFE"]})
}
