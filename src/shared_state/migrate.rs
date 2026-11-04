//! `sessionsation shared migrate --account claude:<name>`: move one account's
//! `projects`, `file-history`, `todos` and `session-env` into the shared store
//! and leave a link behind, without copying data and without any moment in which
//! the account path is missing.
//!
//! Journaled in `operations/shared-migrations/<id>.json`; re-running the same
//! command resumes an unfinished journal and is a no-op once everything is linked.
//!
//! Atomic adoption (the store has no `<name>` yet, first account):
//!
//! ```text
//! 1. symlink  shared/claude/<name> -> <root>/shared/claude/<name>   (placeholder)
//! 2. renameat2(account/<name>, shared/claude/<name>, RENAME_EXCHANGE)
//!    => the real directory sits in the store, the placeholder (now a correct
//!       link to it) sits at the account path.
//! ```
//!
//! One syscall, same inodes: file descriptors held by live Claude processes keep
//! working. Merge (the store already has `<name>`, later accounts): entries are
//! moved by `renameat2(RENAME_NOREPLACE)`, recursing into directories present on
//! both sides; a file present on both sides is a reported conflict, never
//! overwritten. When nothing is left, a temporary link is exchanged with the now
//! empty account directory the same atomic way and late arrivals in the displaced
//! directory are merged again before it is removed.
//!
//! `--fs-path` (design): `--data-dir` stays the canonical logical root, so every
//! link TARGET is `<root>/shared/claude/<name>` as configured (for example
//! `/mnt/user/...`), but all filesystem operations run through a real-disk alias
//! of the same tree (for example `/mnt/disk1/appdata/sessionsation`) because the
//! Unraid user share is a FUSE union that has no `RENAME_EXCHANGE` and cannot
//! reflink. The alias is proven (not trusted) before any change.
use super::*;
use crate::{accounts, daemon};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
};

const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;
const FICLONE: libc::c_ulong = 0x4004_9409;

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub account: String,
    pub dry_run: bool,
    pub allow_full_copy: bool,
    pub fs_path: Option<PathBuf>,
}

/// Maps logical paths under the data root onto the real-disk alias.
struct Fs {
    root: PathBuf,
    base: PathBuf,
}
impl Fs {
    fn io(&self, logical: &Path) -> PathBuf {
        match logical.strip_prefix(&self.root) {
            Ok(rest) => self.base.join(rest),
            Err(_) => logical.to_path_buf(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct NameRec {
    /// pending | done | skipped
    backup: String,
    /// pending | linked | adopted | merged | conflicts
    link: String,
    moved: u64,
    conflicts: Vec<String>,
    backup_files: u64,
    backup_bytes: u64,
    backup_full_copies: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    id: String,
    account: String,
    home: PathBuf,
    fs_path: Option<PathBuf>,
    backup_dir: PathBuf,
    allow_full_copy: bool,
    /// admitted | backing_up | backed_up | linking | complete | incomplete
    phase: String,
    detail: String,
    names: BTreeMap<String, NameRec>,
    created_at: i64,
    updated_at: i64,
}

fn journal_dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::SHARED_MIGRATIONS)
}
fn journal_file(paths: &Paths, id: &str) -> PathBuf {
    journal_dir(paths).join(format!("{id}.json"))
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = daemon::now();
    crate::paths::private_dir(&journal_dir(paths))?;
    atomic_write(&journal_file(paths, &r.id), &serde_json::to_vec_pretty(r)?)
}
fn phase(paths: &Paths, r: &mut Record, name: &str, detail: &str) -> Result<()> {
    r.phase = name.into();
    r.detail = detail.into();
    save(paths, r)?;
    #[cfg(test)]
    super::tests::crash_check(name)?;
    Ok(())
}
fn unfinished(paths: &Paths, account: &str) -> Option<Record> {
    let mut found: Vec<Record> = fs::read_dir(journal_dir(paths))
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<Record>(&fs::read(e.path()).ok()?).ok())
        .filter(|r| r.account == account && r.phase != "complete")
        .collect();
    found.sort_by_key(|r| (r.created_at, r.id.clone()));
    found.pop()
}

/// `YYYYMMDDTHHMMSSZ` from Unix seconds (no calendar dependency).
pub(super) fn stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn is_fuse(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c.as_ptr(), &mut s) == 0 && s.f_type as i64 == FUSE_SUPER_MAGIC }
}
fn nearest_existing(path: &Path) -> PathBuf {
    let mut p = path;
    while fs::symlink_metadata(p).is_err() {
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    p.to_path_buf()
}
fn mkdir_private(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("cannot create {}", path.display()))
}

impl Fs {
    /// Prove that `base` is the same tree as `root` (it cannot be trusted by name).
    fn prove_alias(&self, dry_run: bool) -> Result<&'static str> {
        if self.base == self.root {
            return Ok("same_path");
        }
        ensure!(
            self.base.is_absolute() && fs::metadata(&self.base).is_ok_and(|m| m.is_dir()),
            "SHARED_STATE_FS_PATH_INVALID: --fs-path must be an existing absolute directory"
        );
        if dry_run {
            let read = |p: &Path| {
                let file = p.join(crate::layout::LAYOUT_FILE);
                fs::read(&file)
                    .ok()
                    .zip(fs::metadata(&file).ok().and_then(|m| m.modified().ok()))
            };
            ensure!(
                read(&self.base).is_some() && read(&self.base) == read(&self.root),
                "SHARED_STATE_FS_PATH_MISMATCH: --fs-path does not show the same layout.json as the data root"
            );
            return Ok("layout_file_match");
        }
        let probe = format!(".alias-probe-{}", uuid::Uuid::new_v4());
        let io = self.base.join(&probe);
        let logical = self.root.join(&probe);
        fs::write(&io, b"sessionsation").context("SHARED_STATE_FS_PATH_INVALID: not writable")?;
        let same = fs::read(&logical).is_ok_and(|b| b == b"sessionsation");
        let _ = fs::remove_file(&io);
        ensure!(
            same,
            "SHARED_STATE_FS_PATH_MISMATCH: a file created under --fs-path is not visible under the data root; they are not the same directory tree"
        );
        Ok("probe_file")
    }
}

fn probe_exchange(dir: &Path) -> Result<()> {
    let id = uuid::Uuid::new_v4();
    let (x, y) = (
        dir.join(format!(".probe-{id}-dir")),
        dir.join(format!(".probe-{id}-link")),
    );
    fs::create_dir(&x)?;
    symlink("probe-target", &y)?;
    let result = super::rename_exchange(&x, &y);
    let swapped = fs::symlink_metadata(&x).is_ok_and(|m| m.file_type().is_symlink())
        && fs::symlink_metadata(&y).is_ok_and(|m| m.file_type().is_dir());
    for p in [&x, &y] {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_dir() => {
                let _ = fs::remove_dir(p);
            }
            Ok(_) => {
                let _ = fs::remove_file(p);
            }
            Err(_) => {}
        }
    }
    match result {
        Ok(()) if swapped => Ok(()),
        Ok(()) => bail!(
            "SHARED_STATE_EXCHANGE_UNSUPPORTED: RENAME_EXCHANGE reported success but did not swap"
        ),
        Err(e) => bail!(
            "SHARED_STATE_EXCHANGE_UNSUPPORTED: renameat2(RENAME_EXCHANGE) failed in {} ({e}). On an Unraid user share (FUSE) pass the real-disk path with --fs-path, for example --fs-path /mnt/disk1/appdata/sessionsation",
            dir.display()
        ),
    }
}

fn reflink(src: &fs::File, dst: &fs::File) -> std::io::Result<()> {
    let r = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
fn reflink_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL | libc::EXDEV | libc::ENOSYS)
    )
}
#[cfg(test)]
pub(super) fn reflink_supported(dir: &Path) -> bool {
    probe_reflink(dir).is_ok()
}
fn probe_reflink(dir: &Path) -> Result<()> {
    let id = uuid::Uuid::new_v4();
    let (a, b) = (
        dir.join(format!(".probe-{id}-a")),
        dir.join(format!(".probe-{id}-b")),
    );
    fs::write(&a, vec![7u8; 8192])?;
    let result = (|| {
        let src = fs::File::open(&a)?;
        let dst = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&b)?;
        reflink(&src, &dst)
    })();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    result.map_err(|e| {
        anyhow::anyhow!(
            "BACKUP_REFLINK_UNSUPPORTED: {} cannot reflink ({e}); the backup would be a full copy. Repeat with --allow-full-copy to accept that",
            dir.display()
        )
    })
}

#[derive(Default)]
struct CopyStats {
    files: u64,
    bytes: u64,
    full_copies: u64,
}
/// Reflink copy of a tree (symlinks inside are recreated as symlinks, never followed).
fn copy_tree(src: &Path, dst: &Path, allow_full: bool, stats: &mut CopyStats) -> Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        symlink(fs::read_link(src)?, dst)?;
    } else if meta.is_dir() {
        fs::DirBuilder::new().mode(0o700).create(dst)?;
        let mut children: Vec<_> = fs::read_dir(src)?.collect::<std::io::Result<_>>()?;
        children.sort_by_key(|c| c.file_name());
        for c in children {
            copy_tree(&c.path(), &dst.join(c.file_name()), allow_full, stats)?;
        }
        let _ = fs::set_permissions(
            dst,
            fs::Permissions::from_mode(meta.mode() & 0o7777 | 0o700),
        );
    } else if meta.is_file() {
        let input = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(src)?;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dst)?;
        match reflink(&input, &out) {
            Ok(()) => {}
            Err(e) if reflink_unsupported(&e) && allow_full => {
                let mut input = &input;
                std::io::copy(&mut input, &mut out)?;
                stats.full_copies += 1;
            }
            Err(e) if reflink_unsupported(&e) => bail!(
                "BACKUP_REFLINK_UNSUPPORTED: cannot reflink {} ({e}); repeat with --allow-full-copy to accept a full copy",
                src.display()
            ),
            Err(e) => return Err(e.into()),
        }
        out.flush()?;
        let _ = out.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777 | 0o600));
        if let Ok(m) = meta.modified() {
            let _ = out.set_modified(m);
        }
        stats.files += 1;
        stats.bytes += meta.len();
    }
    Ok(())
}
fn tree_size(path: &Path) -> (u64, u64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if meta.is_dir() {
        fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|c| tree_size(&c.path()))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    } else if meta.is_file() {
        (1, meta.len())
    } else {
        (0, 0)
    }
}

/// Move every non-conflicting entry of `src` into `dst`; returns (moved, conflicts).
/// Never overwrites. A directory present on both sides is merged recursively.
fn merge(src: &Path, dst: &Path, rel: &Path, dry_run: bool) -> Result<(u64, Vec<String>)> {
    let mut moved = 0;
    let mut conflicts = vec![];
    let mut entries: Vec<_> = fs::read_dir(src)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let (s, d, r) = (e.path(), dst.join(e.file_name()), rel.join(e.file_name()));
        let sm = fs::symlink_metadata(&s)?;
        match fs::symlink_metadata(&d) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if dry_run {
                    moved += 1;
                    continue;
                }
                match super::rename_noreplace(&s, &d) {
                    Ok(()) => moved += 1,
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        conflicts.push(r.to_string_lossy().into_owned())
                    }
                    Err(err) if err.raw_os_error() == Some(libc::EXDEV) => bail!(
                        "SHARED_STATE_CROSS_DEVICE: {} and {} are on different file systems",
                        s.display(),
                        d.display()
                    ),
                    Err(err) if matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) => {
                        ensure!(fs::symlink_metadata(&d).is_err(), "RENAME_CONFLICT");
                        fs::rename(&s, &d)?;
                        moved += 1;
                    }
                    Err(err) => return Err(err).context("SHARED_STATE_MERGE_FAILED"),
                }
            }
            Ok(dm) if sm.is_dir() && dm.is_dir() => {
                let (m, c) = merge(&s, &d, &r, dry_run)?;
                moved += m;
                if c.is_empty() && !dry_run {
                    let _ = fs::remove_dir(&s);
                }
                conflicts.extend(c);
            }
            Ok(_) => conflicts.push(r.to_string_lossy().into_owned()),
            Err(err) => return Err(err.into()),
        }
    }
    Ok((moved, conflicts))
}

/// Link one name. `home` and the store are logical; `fs` maps them for I/O.
fn link_name(fs_map: &Fs, root: &Path, home: &Path, name: &str, id: &str) -> Result<NameRec> {
    let mut rec = NameRec {
        backup: "skipped".into(),
        link: "pending".into(),
        ..Default::default()
    };
    let a = home.join(name);
    let a_io = fs_map.io(&a);
    let z = target(root, name);
    let z_io = fs_map.io(&z);
    match state_of(home, name) {
        LinkState::Linked => {
            rec.link = "linked".into();
            return Ok(rec);
        }
        LinkState::Untrusted => bail!(
            "SHARED_STATE_LINK_UNTRUSTED: {} is a symlink or file that is not the shared-store link; it was left unchanged",
            a.display()
        ),
        LinkState::Missing => {
            mkdir_private(&z_io)?;
            symlink(&z, &a_io)?;
            ensure!(
                trusted_link(&a),
                "SHARED_STATE_LINK_UNTRUSTED: {}",
                a.display()
            );
            rec.link = "linked".into();
            return Ok(rec);
        }
        LinkState::EmptyDir | LinkState::Data => {}
    }
    mkdir_private(&fs_map.io(&store(root)))?;
    match fs::symlink_metadata(&z_io) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            adopt(&a_io, &z, &z_io, &a, &mut rec)?
        }
        Ok(m) if m.file_type().is_symlink() => {
            // Only our own interrupted placeholder (a link to itself) is replaced.
            ensure!(
                fs::read_link(&z_io).is_ok_and(|t| t == z),
                "SHARED_STATE_STORE_UNSAFE: {} is a symlink that is not an interrupted migration placeholder",
                z.display()
            );
            fs::remove_file(&z_io)?;
            adopt(&a_io, &z, &z_io, &a, &mut rec)?;
        }
        Ok(m) if m.is_dir() => {
            ensure!(
                private_real_dir(&z_io),
                "SHARED_STATE_STORE_UNSAFE: {} must be a private directory owned by this user",
                z.display()
            );
            let (moved, conflicts) = merge(&a_io, &z_io, Path::new(""), false)?;
            rec.moved += moved;
            if !conflicts.is_empty() {
                rec.link = "conflicts".into();
                rec.conflicts = conflicts;
                return Ok(rec);
            }
            let tmp = a_io.with_file_name(format!(".{name}.shared-link-{id}"));
            if fs::symlink_metadata(&tmp).is_ok() {
                // An interrupted earlier attempt left its temporary link.
                ensure!(
                    fs::read_link(&tmp).is_ok_and(|t| t == z),
                    "SHARED_STATE_STORE_UNSAFE: unexpected {}",
                    tmp.display()
                );
                fs::remove_file(&tmp)?;
            }
            symlink(&z, &tmp)?;
            super::rename_exchange(&tmp, &a_io).context("SHARED_STATE_EXCHANGE_FAILED")?;
            // `tmp` now names the displaced (empty) directory; absorb late arrivals.
            let (late, left) = merge(&tmp, &z_io, Path::new(""), false)?;
            rec.moved += late;
            if left.is_empty() {
                let _ = fs::remove_dir(&tmp);
                if fs::symlink_metadata(&tmp).is_ok() {
                    rec.conflicts
                        .push(format!("(displaced directory kept at {})", tmp.display()));
                }
            } else {
                rec.conflicts.extend(left);
            }
            ensure!(
                trusted_link(&a),
                "SHARED_STATE_LINK_UNTRUSTED: {}",
                a.display()
            );
            rec.link = if rec.conflicts.is_empty() {
                "merged"
            } else {
                "conflicts"
            }
            .into();
        }
        Ok(_) => bail!(
            "SHARED_STATE_STORE_UNSAFE: {} is not a directory",
            z.display()
        ),
        Err(e) => return Err(e.into()),
    }
    Ok(rec)
}

/// First account: one `RENAME_EXCHANGE` puts the real directory into the store
/// and a link to it at the account path.
fn adopt(a_io: &Path, z: &Path, z_io: &Path, a: &Path, rec: &mut NameRec) -> Result<()> {
    symlink(z, z_io)?;
    let swapped = super::rename_exchange(a_io, z_io);
    if let Err(e) = swapped {
        let _ = fs::remove_file(z_io);
        return Err(e).context("SHARED_STATE_EXCHANGE_FAILED");
    }
    ensure!(
        fs::symlink_metadata(z_io).is_ok_and(|m| m.is_dir()),
        "SHARED_STATE_EXCHANGE_FAILED: the store entry is not a directory after the exchange"
    );
    fs::set_permissions(z_io, fs::Permissions::from_mode(0o700))?;
    ensure!(
        trusted_link(a),
        "SHARED_STATE_LINK_UNTRUSTED: {}",
        a.display()
    );
    rec.link = "adopted".into();
    Ok(())
}

fn plan_name(fs_map: &Fs, root: &Path, home: &Path, name: &str) -> Value {
    let a_io = fs_map.io(&home.join(name));
    let z_io = fs_map.io(&target(root, name));
    let state = state_of(home, name);
    let (files, bytes) = tree_size(&a_io);
    let mut v = json!({"state":state.as_str(),"files":files,"bytes":bytes});
    v["action"] = match state {
        LinkState::Linked => json!("none"),
        LinkState::Missing => json!("create_link"),
        LinkState::Untrusted => json!("refuse"),
        LinkState::EmptyDir | LinkState::Data => match fs::symlink_metadata(&z_io) {
            Err(_) => json!("adopt_by_exchange"),
            Ok(m) if m.is_dir() => {
                let (moved, conflicts) =
                    merge(&a_io, &z_io, Path::new(""), true).unwrap_or((0, vec![]));
                v["would_move"] = json!(moved);
                v["conflicts"] = json!(conflicts);
                json!(if conflicts.is_empty() {
                    "merge_then_link"
                } else {
                    "conflicts"
                })
            }
            Ok(_) => json!("refuse"),
        },
    };
    v
}

pub fn migrate(paths: &Paths, opts: &Options) -> Result<Value> {
    validate_name(&opts.account)?;
    let name = opts.account.as_str();
    let account = crate::provider::claude::account(paths, name)?;
    accounts::ensure_available(paths, ProviderId::Claude, name)?;
    let home = account.home.clone();
    let Some((root, _)) = managed_home(&home) else {
        bail!(
            "SHARED_STATE_NOT_APPLICABLE: claude:{name} uses an adopted config directory, which is never linked"
        );
    };
    ensure!(root == paths.root, "SHARED_STATE_NOT_APPLICABLE");
    ensure!(
        fs::symlink_metadata(&home).is_ok_and(|m| m.file_type().is_dir()),
        "UNSAFE_ACCOUNT_HOME: {} is not a directory",
        home.display()
    );
    let fs_map = Fs {
        root: root.clone(),
        base: opts.fs_path.clone().unwrap_or_else(|| root.clone()),
    };
    let alias = fs_map.prove_alias(opts.dry_run)?;
    let home_io = fs_map.io(&home);
    let store_io = fs_map.io(&store(&root));
    let live = crate::provider::claude::live_sessions(&home)
        .map(|l| l.len())
        .unwrap_or(0);

    // ---- static preflight (read only)
    let fuse = is_fuse(&home_io);
    let mut problems = vec![];
    for n in NAMES {
        let a_io = home_io.join(n);
        if fs::symlink_metadata(&a_io).is_ok_and(|m| m.is_dir()) {
            let z_anchor = nearest_existing(&store_io.join(n));
            let (da, dz) = (
                fs::symlink_metadata(&a_io)?.dev(),
                fs::symlink_metadata(&z_anchor)?.dev(),
            );
            if da != dz {
                problems.push(format!(
                    "SHARED_STATE_CROSS_DEVICE: {} (device {da}) and the shared store at {} (device {dz}) are on different file systems; RENAME_EXCHANGE needs one file system. On Unraid the user share may span disks: pass --fs-path of the disk that holds both",
                    a_io.display(),
                    z_anchor.display()
                ));
            }
        }
    }
    let plan: serde_json::Map<String, Value> = NAMES
        .iter()
        .map(|n| ((*n).to_owned(), plan_name(&fs_map, &root, &home, n)))
        .collect();
    let base = json!({"account_id":format!("claude:{name}"),"home":home,"fs_path":opts.fs_path,
        "alias_proof":alias,"fuse_filesystem":fuse,"live_sessions":live,"names":plan});

    if opts.dry_run {
        let conflicts = plan
            .values()
            .any(|v| v["action"] == "conflicts" || v["action"] == "refuse");
        let mut out = base;
        out["dry_run"] = json!(true);
        out["problems"] = json!(problems);
        out["probes"] = json!("skipped_in_dry_run");
        out["would_complete"] = json!(problems.is_empty() && !conflicts);
        out["backup_dir"] = json!(backup_root(&root, daemon::now()).join(name));
        return Ok(out);
    }
    ensure!(problems.is_empty(), "{}", problems.join("; "));
    let _account_lock = accounts::operation_lock(paths, ProviderId::Claude, name)?;
    enable(&root)?;

    // ---- resume or admit
    let mut r = match unfinished(paths, name) {
        Some(r) => r,
        None => {
            if fully_linked(&home) {
                let mut out = base;
                out["already_migrated"] = json!(true);
                out["complete"] = json!(true);
                return Ok(out);
            }
            let ts = daemon::now();
            Record {
                schema_version: 1,
                id: uuid::Uuid::new_v4().to_string(),
                account: name.into(),
                home: home.clone(),
                fs_path: opts.fs_path.clone(),
                backup_dir: backup_root(&root, ts).join(name),
                allow_full_copy: opts.allow_full_copy,
                phase: "admitted".into(),
                detail: "Admitted; probing the file system.".into(),
                names: NAMES
                    .iter()
                    .map(|n| {
                        (
                            (*n).to_owned(),
                            NameRec {
                                backup: "pending".into(),
                                link: "pending".into(),
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
                created_at: ts,
                updated_at: ts,
            }
        }
    };
    r.allow_full_copy = r.allow_full_copy || opts.allow_full_copy;
    r.fs_path = opts.fs_path.clone();
    save(paths, &mut r)?;

    // ---- probes: prove the primitives before changing anything
    mkdir_private(&store_io)?;
    probe_exchange(&store_io).and_then(|()| probe_exchange(&home_io))?;
    let backup_io = fs_map.io(&r.backup_dir);
    mkdir_private(&backup_io)?;
    if !r.allow_full_copy {
        probe_reflink(&backup_io)?;
    }

    // ---- backup
    phase(
        paths,
        &mut r,
        "backing_up",
        "Taking reflink backups of the account directories.",
    )?;
    for n in NAMES {
        let a_io = home_io.join(n);
        let rec = r.names.get_mut(n).expect("every name is journaled");
        if rec.backup == "done" || rec.backup == "skipped" {
            continue;
        }
        if !fs::symlink_metadata(&a_io).is_ok_and(|m| m.is_dir()) {
            rec.backup = "skipped".into();
            continue;
        }
        let dst = backup_io.join(n);
        if fs::symlink_metadata(&dst).is_ok() {
            // Our own partial copy from an interrupted run.
            fs::remove_dir_all(&dst)?;
        }
        let mut stats = CopyStats::default();
        copy_tree(&a_io, &dst, r.allow_full_copy, &mut stats)?;
        rec.backup = "done".into();
        rec.backup_files = stats.files;
        rec.backup_bytes = stats.bytes;
        rec.backup_full_copies = stats.full_copies;
        save(paths, &mut r)?;
    }
    phase(paths, &mut r, "backed_up", "Backups complete; linking.")?;

    // ---- link
    phase(
        paths,
        &mut r,
        "linking",
        "Replacing account directories with links to the shared store.",
    )?;
    let lock = lock_file(&store(&root).join(LOCK))?;
    lock.lock_exclusive()?;
    for n in NAMES {
        if r.names[n].link == "linked"
            || r.names[n].link == "adopted"
            || r.names[n].link == "merged"
        {
            continue;
        }
        let done = link_name(&fs_map, &root, &home, n, &r.id)?;
        let rec = r.names.get_mut(n).expect("journaled");
        rec.moved += done.moved;
        rec.link = done.link;
        rec.conflicts = done.conflicts;
        save(paths, &mut r)?;
    }
    drop(lock);
    let complete = NAMES
        .iter()
        .all(|n| matches!(r.names[*n].link.as_str(), "linked" | "adopted" | "merged"))
        && fully_linked(&home);
    if complete {
        phase(
            paths,
            &mut r,
            "complete",
            "All four directories are links to the shared store.",
        )?;
    } else {
        phase(
            paths,
            &mut r,
            "incomplete",
            "Some entries conflict with the shared store and were left in place; resolve them and repeat the command.",
        )?;
    }
    let mut out = base;
    out["dry_run"] = json!(false);
    out["operation_id"] = json!(r.id);
    out["phase"] = json!(r.phase);
    out["complete"] = json!(complete);
    out["backup_dir"] = json!(r.backup_dir);
    out["backup_mode"] = json!(if r.names.values().any(|n| n.backup_full_copies > 0) {
        "includes_full_copies"
    } else {
        "reflink"
    });
    out["results"] = json!(r.names);
    if !complete {
        out["next"] = json!(
            "Entries listed under conflicts exist in both the account directory and the shared store. Nothing was overwritten. Move or merge them by hand, then run the same command again."
        );
    }
    Ok(out)
}

fn backup_root(root: &Path, ts: i64) -> PathBuf {
    root.join("backups")
        .join(format!("shared-migrate-{}", stamp(ts)))
}
