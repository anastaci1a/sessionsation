//! `sessionsation layout status|migrate|rollback`: convert a legacy (version 1)
//! data root to the version 2 layout, and back.
//!
//! Safety rules:
//! - Runs only with the supervisor stopped: it holds `daemon.lock` (the lock
//!   every supervisor build holds) and `layout.lock` for its whole run.
//! - Takes a metadata backup first (`--backup DIR`; account homes are moved by
//!   rename only and are never copied, so credentials never leave the root).
//! - Moves only by `rename(2)` inside the root; nothing is rewritten or merged.
//!   A destination that already exists is a conflict and stops the run before
//!   any change.
//! - An account home that any process uses (environment, command line, working
//!   directory, executable or open file under the home) is never moved: it is
//!   deferred, stays at its legacy path, and the reason is recorded. A later
//!   run moves it once idle. A moved home leaves a relative compatibility
//!   symlink at its legacy path, so absolute paths recorded inside provider
//!   state (for example Codex rollout paths) keep resolving.
//! - Idempotent and resumable: every step checks what is already done, and
//!   `layout.json` records `migrating` until the run completes.
//! - Reversible: `layout rollback` moves everything back to the version 1
//!   names (refusing homes in use) and removes the version 2 record.
use super::{
    ACCOUNTS, COMPAT_LINKS, Deferred, LAYOUT_FILE, LAYOUT_LOCK, LEGACY_HOME_PARENTS, MOVES,
    REGISTRY_DB, Record, State, VERSION, legacy_home, now, read_record, target_home, write_record,
};
use crate::{
    paths::{lock_file, private_dir},
    provider::ProviderId,
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

/// Content of the file left at the legacy `registry.db` path. It is not a
/// SQLite database, so an older build fails closed ("file is not a database")
/// instead of creating an empty shadow registry.
pub const TRIPWIRE: &str = "SESSIONSATION LAYOUT 2: this data root was migrated; the registry is registry/registry.db. Older builds must not use it (run `sessionsation layout rollback` with the newer build first).\n";

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub backup: Option<PathBuf>,
    pub dry_run: bool,
}

struct Locks {
    _layout: fs::File,
    _daemon: Option<fs::File>,
}
/// `layout.lock` always; `daemon.lock` unless only planning (a dry run reads
/// and never changes anything, so it may run beside a live supervisor).
fn lock(root: &Path, change: bool) -> Result<Locks> {
    let layout = lock_file(&root.join(LAYOUT_LOCK))?;
    layout
        .try_lock_exclusive()
        .context("LAYOUT_BUSY: another layout command is running")?;
    let daemon = if change {
        let daemon = lock_file(&root.join("daemon.lock"))?;
        daemon.try_lock_exclusive().context(
            "DAEMON_RUNNING: stop the supervisor (`sessionsation daemon stop`) before changing the data-root layout",
        )?;
        Some(daemon)
    } else {
        None
    };
    Ok(Locks {
        _layout: layout,
        _daemon: daemon,
    })
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}
fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}
fn real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
}
fn is_tripwire(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
        && fs::read(p).is_ok_and(|b| b == TRIPWIRE.as_bytes())
}
fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}
/// Relative symlink target from `link` to `target` (both under `root`).
fn relative_target(root: &Path, link: &Path, target: &Path) -> PathBuf {
    let depth = link
        .strip_prefix(root)
        .map(|p| p.components().count().saturating_sub(1))
        .unwrap_or(0);
    let mut out = PathBuf::new();
    for _ in 0..depth {
        out.push("..");
    }
    out.push(target.strip_prefix(root).unwrap_or(target));
    out
}
fn rename(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        private_dir(parent)?;
    }
    fs::rename(from, to).with_context(|| format!("move {} -> {}", from.display(), to.display()))?;
    if let Some(parent) = to.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

// ------------------------------------------------------------------ in use

/// Processes that use `home`: an environment value, command-line argument,
/// working directory, executable or open file names a path under it (as given
/// or canonical). Conservative: a substring match counts. Excludes this process.
pub fn home_users(home: &Path) -> Vec<String> {
    let mut needles = vec![home.to_string_lossy().into_owned()];
    if let Ok(c) = fs::canonicalize(home) {
        let c = c.to_string_lossy().into_owned();
        if !needles.contains(&c) {
            needles.push(c);
        }
    }
    let names = |s: &str| {
        needles.iter().any(|n| {
            s.match_indices(n.as_str()).any(|(i, _)| {
                matches!(
                    s.as_bytes().get(i + n.len()),
                    None | Some(b'/' | b'\0' | b' ' | b'"' | b'\'' | b':' | b';' | b',' | b'=')
                )
            })
        })
    };
    let me = std::process::id().to_string();
    let mut users = vec![];
    let Ok(procs) = fs::read_dir("/proc") else {
        return vec!["process table unreadable".into()];
    };
    for p in procs.flatten() {
        let pid = p.file_name().to_string_lossy().into_owned();
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dir = p.path();
        let comm = fs::read_to_string(dir.join("comm"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let mut why = vec![];
        if let Ok(env) = fs::read(dir.join("environ")) {
            for var in env.split(|b| *b == 0) {
                let var = String::from_utf8_lossy(var);
                if let Some((k, v)) = var.split_once('=')
                    && names(v)
                {
                    why.push(format!("env {k}"));
                    break;
                }
            }
        }
        if let Ok(cmd) = fs::read(dir.join("cmdline"))
            && cmd
                .split(|b| *b == 0)
                .any(|a| names(&String::from_utf8_lossy(a)))
        {
            why.push("command line".into());
        }
        for (link, label) in [("cwd", "working directory"), ("exe", "executable")] {
            if fs::read_link(dir.join(link)).is_ok_and(|t| names(&t.to_string_lossy())) {
                why.push(label.into());
            }
        }
        if let Ok(fds) = fs::read_dir(dir.join("fd"))
            && fds
                .flatten()
                .any(|f| fs::read_link(f.path()).is_ok_and(|t| names(&t.to_string_lossy())))
        {
            why.push("open file".into());
        }
        if !why.is_empty() {
            users.push(format!("pid {pid} {comm}: {}", why.join(", ")));
        }
    }
    users
}

// ------------------------------------------------------------------- plan

#[derive(Debug, Default)]
struct Plan {
    /// (legacy relative, new relative) store moves still to do.
    stores: Vec<(String, String)>,
    /// Compatibility links still to create (legacy relative).
    links: Vec<String>,
    tripwire: bool,
    /// Idle homes to move: (provider, name, legacy, target).
    homes: Vec<(ProviderId, String, PathBuf, PathBuf)>,
    deferred: Vec<Deferred>,
    conflicts: Vec<String>,
}
impl Plan {
    fn is_empty(&self) -> bool {
        self.stores.is_empty() && self.links.is_empty() && !self.tripwire && self.homes.is_empty()
    }
    fn json(&self) -> Value {
        json!({
            "stores": self.stores.iter().map(|(a, b)| json!({"from":a,"to":b})).collect::<Vec<_>>(),
            "compat_links": self.links,
            "registry_tripwire": self.tripwire,
            "homes": self.homes.iter().map(|(p, n, l, t)| json!({"provider":p.as_str(),"name":n,"from":l,"to":t})).collect::<Vec<_>>(),
            "deferred": self.deferred,
            "conflicts": self.conflicts,
        })
    }
}

/// Legacy managed homes present as real directories: (provider, name, path).
fn legacy_homes(root: &Path) -> Vec<(ProviderId, String, PathBuf)> {
    let mut homes = vec![];
    let mut scan = |provider: ProviderId, dir: PathBuf| {
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        let mut found: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| crate::paths::validate_name(n).is_ok())
            .filter(|n| provider != ProviderId::Codex || !super::is_platform_dir(n))
            .collect();
        found.sort();
        for name in found {
            homes.push((provider, name.clone(), dir.join(&name)));
        }
    };
    scan(ProviderId::Codex, root.join(ACCOUNTS));
    for (provider, parent) in LEGACY_HOME_PARENTS {
        scan(*provider, root.join(parent));
    }
    homes
}

fn plan(root: &Path, record: Option<&Record>) -> Plan {
    let mut plan = Plan::default();
    for (legacy, new) in MOVES {
        let from = root.join(legacy);
        let to = root.join(new);
        if !exists(&from) || is_symlink(&from) || (*legacy == "registry.db" && is_tripwire(&from)) {
            continue;
        }
        if exists(&to) {
            plan.conflicts
                .push(format!("{legacy} and {new} both exist; move or remove one"));
        } else {
            plan.stores.push(((*legacy).into(), (*new).into()));
        }
    }
    // Links and the tripwire belong to entries this migration moved (or an
    // unfinished one may have moved); a root created as version 2 gets none.
    let unfinished = record.is_none_or(|r| r.state == State::Migrating || r.version < VERSION);
    let was_moved = |legacy: &str| {
        plan.stores.iter().any(|(l, _)| l == legacy)
            || unfinished
            || record.is_some_and(|r| r.moved.iter().any(|(l, _)| l == legacy))
    };
    let mut links = vec![];
    for legacy in COMPAT_LINKS {
        let new = MOVES.iter().find(|(l, _)| l == legacy).map(|(_, n)| *n);
        let will_move = plan.stores.iter().any(|(l, _)| l == legacy);
        let moved_before =
            new.is_some_and(|n| exists(&root.join(n))) && !exists(&root.join(legacy));
        if will_move || (moved_before && was_moved(legacy)) {
            links.push((*legacy).into());
        }
    }
    plan.links = links;
    plan.tripwire = plan.stores.iter().any(|(l, _)| l == "registry.db")
        || (was_moved("registry.db")
            && !exists(&root.join("registry.db"))
            && exists(&root.join(REGISTRY_DB)));
    for platform in ProviderId::ALL {
        let dir = root.join(ACCOUNTS).join(platform.as_str());
        if ["auth.json", "config.toml", ".switcher-lease", "sessions"]
            .iter()
            .any(|f| exists(&dir.join(f)))
        {
            plan.conflicts.push(format!(
                "accounts/{} looks like a legacy Codex home named after a platform; rename that account first",
                platform.as_str()
            ));
        }
    }
    let previous: Vec<Deferred> = record.map(|r| r.deferred.clone()).unwrap_or_default();
    for (provider, name, legacy) in legacy_homes(root) {
        let target = target_home(root, provider, &name);
        if exists(&target) {
            plan.conflicts.push(format!(
                "{} and {} both exist",
                rel(root, &legacy),
                rel(root, &target)
            ));
            continue;
        }
        let users = home_users(&legacy);
        if users.is_empty() {
            plan.homes.push((provider, name, legacy, target));
        } else {
            let since = previous
                .iter()
                .find(|d| d.provider == provider.as_str() && d.name == name)
                .map_or_else(now, |d| d.since);
            plan.deferred.push(Deferred {
                provider: provider.as_str().into(),
                name,
                legacy: rel(root, &legacy),
                target: rel(root, &target),
                reason: format!(
                    "in use: {}",
                    users.iter().take(4).cloned().collect::<Vec<_>>().join("; ")
                ),
                since,
            });
        }
    }
    plan
}

// ----------------------------------------------------------------- backup

fn vacuum_into(db: &Path, out: &Path) -> Result<()> {
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    c.execute("VACUUM INTO ?1", [out.to_string_lossy()])?;
    Ok(())
}

/// Metadata backup: every top-level entry except account homes, with SQLite
/// databases copied consistently (`VACUUM INTO`). Homes are recorded by path
/// and inode only.
fn backup(root: &Path, dir: &Path, record: Option<&Record>) -> Result<Value> {
    if exists(dir) {
        ensure!(
            fs::read_dir(dir)?.next().is_none(),
            "BACKUP_NOT_EMPTY: {} already has content; choose a new directory",
            dir.display()
        );
    }
    private_dir(dir)?;
    let skip = |name: &str| {
        name == ACCOUNTS
            || LEGACY_HOME_PARENTS.iter().any(|(_, p)| *p == name)
            || name.starts_with("registry.db")
            || name == LAYOUT_LOCK
            || name == "daemon.lock"
    };
    for entry in fs::read_dir(root)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if skip(&name) {
            continue;
        }
        let status = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto", "--"])
            .arg(entry.path())
            .arg(dir.join(&name))
            .status()
            .context("BACKUP_FAILED: cp is unavailable")?;
        ensure!(status.success(), "BACKUP_FAILED: copying {name}");
    }
    let mut databases = vec![];
    for db in ["registry.db", REGISTRY_DB, super::RECEIPTS_DB] {
        let from = root.join(db);
        if !real_file(&from) || is_tripwire(&from) {
            continue;
        }
        let to = dir.join(db);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = fs::remove_file(format!("{}{suffix}", to.display()));
        }
        if let Some(parent) = to.parent() {
            private_dir(parent)?;
        }
        vacuum_into(&from, &to).with_context(|| format!("BACKUP_FAILED: {db}"))?;
        databases.push(db);
    }
    use std::os::unix::fs::MetadataExt;
    let homes: Vec<Value> = legacy_homes(root)
        .into_iter()
        .chain(ProviderId::ALL.iter().flat_map(|p| {
            fs::read_dir(root.join(ACCOUNTS).join(p.as_str()))
                .into_iter()
                .flatten()
                .flatten()
                .map(move |e| (*p, e.file_name().to_string_lossy().into_owned(), e.path()))
        }))
        .map(|(p, n, h)| {
            let m = fs::symlink_metadata(&h).ok();
            json!({"provider":p.as_str(),"name":n,"path":h,"inode":m.as_ref().map(|m| m.ino()),"symlink":m.is_some_and(|m| m.file_type().is_symlink())})
        })
        .collect();
    let manifest = json!({"root":root,"created_at":now(),"databases":databases,"homes":homes,"layout_before":record.map(|r| serde_json::to_value(r).unwrap_or_default())});
    crate::paths::atomic_write(
        &dir.join("layout-backup.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(manifest)
}
fn real_file(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

fn checkpoint(db: &Path) -> Result<()> {
    if real_file(db) && !is_tripwire(db) {
        let c = rusqlite::Connection::open(db)?;
        c.busy_timeout(std::time::Duration::from_secs(10))?;
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------- migrate

pub fn migrate(root: &Path, options: &Options) -> Result<Value> {
    let _locks = lock(root, !options.dry_run)?;
    let mut record = read_record(root)?.unwrap_or(Record {
        version: 1,
        ..Record::default()
    });
    ensure!(
        record.version <= VERSION,
        "LAYOUT_UNSUPPORTED: layout version {} is newer than this build",
        record.version
    );
    ensure!(
        record.state != State::RollingBack,
        "LAYOUT_MIGRATION_IN_PROGRESS: a rollback did not finish; run `sessionsation layout rollback` again"
    );
    let plan = plan(root, Some(&record));
    if !plan.conflicts.is_empty() {
        bail!("LAYOUT_CONFLICT: {}", plan.conflicts.join("; "));
    }
    let summary = plan.json();
    if options.dry_run {
        return Ok(
            json!({"dry_run":true,"layout_version":record.version,"state":record.state,"plan":summary}),
        );
    }
    let fresh = record.version < VERSION || record.state == State::Migrating;
    let mut backup_manifest = Value::Null;
    if !plan.is_empty() {
        let dir = options.backup.as_deref().context(
            "BACKUP_REQUIRED: pass --backup DIR (a new or empty directory) before the layout changes",
        )?;
        backup_manifest = backup(root, dir, read_record(root)?.as_ref())?;
        record.backup = Some(dir.to_string_lossy().into_owned());
    }
    if fresh {
        record.state = State::Migrating;
        write_record(root, &record)?;
    }
    // Stores: the registry WAL is folded into the database first so the three
    // files can never disagree after the move.
    if plan.stores.iter().any(|(l, _)| l == "registry.db") {
        checkpoint(&root.join("registry.db"))?;
    }
    for (legacy, new) in &plan.stores {
        let (from, to) = (root.join(legacy), root.join(new));
        if exists(&from) && !exists(&to) {
            rename(&from, &to)?;
        }
        if legacy == "registry.db" {
            for suffix in ["-wal", "-shm"] {
                let (f, t) = (
                    PathBuf::from(format!("{}{suffix}", from.display())),
                    PathBuf::from(format!("{}{suffix}", to.display())),
                );
                if exists(&f) && !exists(&t) {
                    rename(&f, &t)?;
                }
            }
        }
        let entry = (legacy.clone(), new.clone());
        if !record.moved.contains(&entry) {
            record.moved.push(entry);
        }
    }
    for legacy in &plan.links {
        let link = root.join(legacy);
        let new = MOVES
            .iter()
            .find(|(l, _)| l == legacy)
            .map(|(_, n)| *n)
            .context("link")?;
        if !exists(&link) {
            symlink(relative_target(root, &link, &root.join(new)), &link)?;
        }
        if !record.compat_links.contains(legacy) {
            record.compat_links.push(legacy.clone());
        }
    }
    if plan.tripwire && !exists(&root.join("registry.db")) {
        crate::paths::atomic_write(&root.join("registry.db"), TRIPWIRE.as_bytes())?;
    }
    super::ensure_dirs(root)?;
    // Homes: re-check right before each move; a home that became busy is deferred.
    let mut deferred = plan.deferred.clone();
    let mut moved_homes = vec![];
    for (provider, name, legacy, target) in &plan.homes {
        let users = home_users(legacy);
        if !users.is_empty() {
            deferred.push(Deferred {
                provider: provider.as_str().into(),
                name: name.clone(),
                legacy: rel(root, legacy),
                target: rel(root, target),
                reason: format!("in use: {}", users.join("; ")),
                since: now(),
            });
            continue;
        }
        super::ensure_home_parent(root, *provider)?;
        rename(legacy, target)?;
        symlink(relative_target(root, legacy, target), legacy)?;
        let l = rel(root, legacy);
        record.moved.push((l.clone(), rel(root, target)));
        record.compat_links.push(l);
        moved_homes.push(json!({"provider":provider.as_str(),"name":name,"to":rel(root, target)}));
    }
    record.deferred = deferred;
    record.version = VERSION;
    record.state = State::Complete;
    if fresh {
        record.migrated_at = Some(now());
    }
    record.history.push(json!({"at":now(),"action":if fresh {"migrate"} else {"resume"},"stores":plan.stores.len(),"homes_moved":moved_homes.len(),"deferred":record.deferred.len()}));
    write_record(root, &record)?;
    super::admit(root)?;
    verify(root)?;
    Ok(json!({
        "layout_version": VERSION,
        "state": record.state,
        "stores_moved": plan.stores.len(),
        "homes_moved": moved_homes,
        "deferred": record.deferred,
        "compat_links": record.compat_links,
        "backup": record.backup,
        "backup_databases": backup_manifest["databases"],
    }))
}

/// The migrated registry opens and holds its state row.
fn verify(root: &Path) -> Result<()> {
    let db = root.join(REGISTRY_DB);
    if real_file(&db) {
        let c =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='state'",
                [],
                |r| r.get(0),
            )
            .context("LAYOUT_VERIFY_FAILED: registry/registry.db is unreadable")?;
        ensure!(n == 1, "LAYOUT_VERIFY_FAILED: registry has no state table");
    }
    Ok(())
}

// --------------------------------------------------------------- rollback

pub fn rollback(root: &Path, dry_run: bool) -> Result<Value> {
    let _locks = lock(root, !dry_run)?;
    let Some(mut record) = read_record(root)? else {
        return Ok(
            json!({"rolled_back":false,"detail":"no layout record: the root already uses the legacy layout"}),
        );
    };
    // Homes now at accounts/<platform>/<name> go back to their legacy paths.
    let mut homes = vec![];
    let mut conflicts = vec![];
    for provider in ProviderId::ALL {
        let dir = root.join(ACCOUNTS).join(provider.as_str());
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let target = e.path();
            if !real_dir(&target) {
                continue;
            }
            let Some(legacy) = legacy_home(root, provider, &name) else {
                conflicts.push(format!(
                    "codex account '{name}' has no legacy path (platform name)"
                ));
                continue;
            };
            if real_dir(&legacy) || (exists(&legacy) && !is_symlink(&legacy)) {
                conflicts.push(format!(
                    "{} and {} both exist",
                    rel(root, &legacy),
                    rel(root, &target)
                ));
                continue;
            }
            let users = home_users(&target);
            if !users.is_empty() {
                conflicts.push(format!(
                    "{} is in use ({}); stop it first",
                    rel(root, &target),
                    users.join("; ")
                ));
                continue;
            }
            homes.push((target, legacy));
        }
    }
    let mut stores = vec![];
    for (legacy, new) in MOVES.iter().rev() {
        let (from, to) = (root.join(new), root.join(legacy));
        if !exists(&from) {
            continue;
        }
        let replaceable =
            !exists(&to) || is_symlink(&to) || (*legacy == "registry.db" && is_tripwire(&to));
        if replaceable {
            stores.push((from, to));
        } else {
            conflicts.push(format!("{legacy} and {new} both exist"));
        }
    }
    if !conflicts.is_empty() {
        bail!("LAYOUT_CONFLICT: {}", conflicts.join("; "));
    }
    let plan = json!({
        "homes": homes.iter().map(|(a, b)| json!({"from":rel(root, a),"to":rel(root, b)})).collect::<Vec<_>>(),
        "stores": stores.iter().map(|(a, b)| json!({"from":rel(root, a),"to":rel(root, b)})).collect::<Vec<_>>(),
    });
    if dry_run {
        return Ok(json!({"dry_run":true,"plan":plan}));
    }
    record.state = State::RollingBack;
    write_record(root, &record)?;
    for (target, legacy) in &homes {
        if is_symlink(legacy) {
            fs::remove_file(legacy)?;
        }
        rename(target, legacy)?;
    }
    checkpoint(&root.join(REGISTRY_DB))?;
    for (from, to) in &stores {
        if is_symlink(to) || is_tripwire(to) {
            fs::remove_file(to)?;
        }
        rename(from, to)?;
        if to.ends_with("registry.db") {
            for suffix in ["-wal", "-shm"] {
                let f = PathBuf::from(format!("{}{suffix}", from.display()));
                if exists(&f) {
                    rename(&f, &PathBuf::from(format!("{}{suffix}", to.display())))?;
                }
            }
        }
    }
    // Remaining compatibility links whose target is gone, and empty v2 dirs.
    for legacy in COMPAT_LINKS {
        let p = root.join(legacy);
        if is_symlink(&p) && !p.exists() {
            fs::remove_file(&p)?;
        }
    }
    for provider in ProviderId::ALL {
        let _ = fs::remove_dir(root.join(ACCOUNTS).join(provider.as_str()));
    }
    for dir in [
        super::CACHE,
        super::LOGS,
        super::OPERATIONS,
        super::REGISTRY,
        "sessions/archived",
        "sessions/preserved",
        "sessions/hosted",
    ] {
        let _ = fs::remove_dir(root.join(dir));
    }
    fs::remove_file(root.join(LAYOUT_FILE))?;
    fs::File::open(root)?.sync_all()?;
    Ok(json!({"rolled_back":true,"plan":plan,"backup":record.backup}))
}

// ----------------------------------------------------------------- status

pub fn status(root: &Path) -> Result<Value> {
    let record = read_record(root)?;
    let legacy = super::legacy_entries(root);
    let deferred: Vec<Value> = record
        .as_ref()
        .map(|r| {
            r.deferred
                .iter()
                .map(|d| {
                    let users = home_users(&root.join(&d.legacy));
                    json!({"provider":d.provider,"name":d.name,"legacy":d.legacy,"target":d.target,"reason":d.reason,"since":d.since,"in_use_now":!users.is_empty()})
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "layout_version": record.as_ref().map_or(1, |r| r.version),
        "state": record.as_ref().map(|r| r.state.clone()),
        "migration_required": record.is_none() && !legacy.is_empty(),
        "legacy_entries": legacy,
        "deferred": deferred,
        "compat_links": record.as_ref().map(|r| r.compat_links.clone()).unwrap_or_default(),
        "backup": record.as_ref().and_then(|r| r.backup.clone()),
        "migrated_at": record.as_ref().and_then(|r| r.migrated_at),
    }))
}
