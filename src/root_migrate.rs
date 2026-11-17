//! `sessionsation root status | migrate | rollback | finalize` (contract 1.54.0):
//! re-key a data root and its sessions from one spelling of a bind-mounted tree
//! (`/mnt/user/...`) to the other (`/mnt/disk1/...`).
//!
//! Two tiers:
//! - **Tier R** (`root migrate --from F --to T --backup DIR`, supervisor stopped):
//!   proves the alias, takes a backup, stamps layout version 3, re-points the
//!   shared-store links, rewrites current-state registry records (journaled per
//!   value in `<run>.changes.jsonl`), merges Claude memory per key, re-keys every
//!   Claude session that is neither running nor archived (state `rekeyed`),
//!   records new thread locations for Codex threads that are not running, then
//!   activates the alias. Running sessions are not stopped: they keep their old
//!   environment and are matched through [`crate::alias::logical`]
//!   (`deferred_running`).
//! - **Tier S** (`root migrate --session S`, supervisor running): one session;
//!   a running Claude session is stopped, re-keyed, resumed and verified by a
//!   detached worker ([`session`]).
//!
//! Every transcript is copied outside `projects/` and the copy verified
//! (sha256) BEFORE its rename; the rename happens only while the session is
//! stopped. Retained copies live under the `--backup` directory and no 1.54
//! command deletes them (`finalize` neither). Memory is merged per file and a
//! conflict blocks that session without choosing a winner.
//!
//! Journals: `operations/root-migrations/<run>.json` (+ `.changes.jsonl`), one
//! `session-<catalog id>.json` per session, `memory-bases.json`. Everything is
//! idempotent and resumable; `root rollback` restores layout version 2 and the
//! original records.
pub mod keys;
pub mod runtime;
pub mod session;
#[cfg(test)]
mod tests;

use crate::{
    alias::{self, Alias, Entry},
    catalog::{self, Session},
    layout::{self, RootAlias, State},
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{ProviderId, claude, native},
};
use anyhow::{Context, Result, bail, ensure};
use keys::{Found, MemoryResult, Moved, Retained};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const STEPS: &[&str] = &[
    "admitted", "backup", "mark", "links", "registry", "claude", "codex", "activate", "verify",
    "complete",
];
pub const SESSION_STATES: &[&str] = &[
    "verified",
    "rekeyed",
    "archived_rekey_at_restore",
    "deferred_running",
    "left_behind",
    "unsupported_provider",
    "not_applicable",
    "verify_failed",
    "failed",
    "uncertain",
    "relocating",
];
pub const ERRORS: &[&str] = &[
    "ROOT_ALIAS_UNPROVEN",
    "ROOT_ALIAS_UNVERIFIED",
    "ROOT_MIGRATION_BLOCKED",
    "ROOT_MIGRATION_MISMATCH",
    "ROOT_REKEY_CONFLICT",
    "ROOT_REKEY_UNSUPPORTED",
    "ROOT_REKEY_UNSUPPORTED_PROVIDER",
    "ROOT_VERIFY_FAILED",
    "ROOT_ROLLBACK_BLOCKED",
    "BACKUP_REFLINK_UNSUPPORTED",
    "BACKUP_VERIFY_FAILED",
    "SESSION_STATE_UNVERIFIED",
    "SESSION_DELIVERY_IN_PROGRESS",
    "BACKUP_REQUIRED",
    "BACKUP_NOT_EMPTY",
];

fn step_index(step: &str) -> usize {
    STEPS.iter().position(|s| *s == step).unwrap_or(0)
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn now() -> i64 {
    crate::daemon::now()
}

// ----------------------------------------------------------------- journals

pub(crate) fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(layout::ROOT_MIGRATIONS)
}
fn run_file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
fn changes_file(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(run_file(paths, id)?.with_extension("changes.jsonl"))
}
pub(crate) fn session_file(paths: &Paths, catalog_id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(catalog_id).context("INVALID_SESSION_ID")?;
    Ok(dir(paths).join(format!("session-{catalog_id}.json")))
}
fn bases_file(paths: &Paths) -> PathBuf {
    dir(paths).join("memory-bases.json")
}

/// One session whose catalog says it is attached (or that is recognised as
/// running) when Tier R starts: the F2 rollback gate compares it after activation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GateRow {
    pub session_id: String,
    pub name: String,
    pub space: Option<String>,
    pub provider: String,
    pub account: String,
    pub conversation_id: String,
    pub pane: Option<String>,
    pub pid: Option<u32>,
    pub running_before: bool,
    /// The probe could not decide (the gate then fails for this row).
    #[serde(default)]
    pub probe_error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkChange {
    pub home: PathBuf,
    pub name: String,
    pub old: PathBuf,
    pub new: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaudeJson {
    pub home: PathBuf,
    /// (new key, sha256 of the cloned raw value)
    pub added: Vec<(String, String)>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Run {
    pub schema_version: u32,
    pub id: String,
    pub kind: String,
    pub from: PathBuf,
    pub to: PathBuf,
    pub backup: PathBuf,
    pub allow_full_copy: bool,
    pub entries: Vec<Entry>,
    pub unmapped: Vec<String>,
    pub step: String,
    /// running | complete | verify_failed | rolled_back
    pub state: String,
    pub detail: String,
    pub root_id: String,
    pub canonical_root: PathBuf,
    pub previous_root: PathBuf,
    pub layout_before: Value,
    pub layout_before_sha256: String,
    pub gate_before: Vec<GateRow>,
    pub links: Vec<LinkChange>,
    pub claude_json: Vec<ClaudeJson>,
    pub memory_conflict_keys: Vec<String>,
    pub verify: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub activated_at: Option<i64>,
}
fn save_run(paths: &Paths, r: &mut Run) -> Result<()> {
    r.updated_at = now();
    private_dir(&dir(paths))?;
    atomic_write(&run_file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
fn step(paths: &Paths, r: &mut Run, name: &str, detail: &str) -> Result<()> {
    // Death after the step's work, before its journal save (tests).
    #[cfg(test)]
    tests::die_check(&format!("before:{name}"));
    r.step = name.into();
    r.detail = detail.into();
    save_run(paths, r)?;
    #[cfg(test)]
    tests::die_check(name);
    #[cfg(test)]
    tests::crash_check(name)?;
    Ok(())
}
pub(crate) fn runs(paths: &Paths) -> Vec<Run> {
    let mut out: Vec<Run> = fs::read_dir(dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.ends_with(".json") && uuid::Uuid::parse_str(n.trim_end_matches(".json")).is_ok()
        })
        .filter_map(|e| serde_json::from_slice::<Run>(&fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by_key(|r| (r.created_at, r.id.clone()));
    out
}
fn latest_run(paths: &Paths) -> Option<Run> {
    runs(paths).pop()
}

/// One value change of a rewritten registry record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub file: String,
    pub pointer: String,
    pub old: Value,
    pub new: Value,
    pub before_sha256: String,
    pub after_sha256: String,
}
fn log_change(paths: &Paths, run: &str, c: &Change) -> Result<()> {
    let path = changes_file(paths, run)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    let mut line = serde_json::to_vec(c)?;
    line.push(b'\n');
    f.write_all(&line)?;
    f.sync_all()?;
    Ok(())
}
fn read_changes(paths: &Paths, run: &str) -> Vec<Change> {
    fs::read_to_string(changes_file(paths, run).unwrap_or_default())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Per-session record (`session-<catalog id>.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRec {
    pub schema_version: u32,
    /// The catalog id (lifecycle admission names it).
    pub id: String,
    pub kind: String,
    pub session_id: String,
    pub name: String,
    pub space: Option<String>,
    pub provider: String,
    pub account: String,
    /// Top level so lifecycle admission blocks other operations on the conversation.
    pub conversation_id: String,
    pub conversation_ids: Vec<String>,
    pub run: String,
    /// Reported state (see [`SESSION_STATES`]).
    pub state: String,
    pub reason: Option<String>,
    /// Worker phase; `complete` whenever no worker owns the record.
    pub phase: String,
    pub detail: String,
    pub uncertain_from: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub key_from: Option<String>,
    pub key_to: Option<String>,
    pub memory_key: Option<String>,
    pub found: Vec<Found>,
    pub retained_dir: Option<PathBuf>,
    pub retained: Vec<Retained>,
    pub moved: Vec<Moved>,
    pub memory: Option<MemoryResult>,
    pub collisions: Vec<String>,
    pub revision_before: Option<u64>,
    pub revision_after: Option<u64>,
    /// Codex: the previous thread-location record (`None` = absent).
    pub codex_location_before: Option<String>,
    pub codex_location_written: bool,
    /// Codex: the exact record this tool wrote (rollback restores only over it).
    pub codex_location_after: Option<String>,
    pub codex_job: Option<String>,
    pub codex_rollout: Option<(PathBuf, u64, String)>,
    /// The Claude account home the session's files live in (fixed at admission).
    pub home: Option<PathBuf>,
    /// The worker undoes the re-key (`root rollback --session` of a running session).
    pub rollback: bool,
    /// The worker stopped the session's process: any later failure resumes it.
    pub stopped_by_worker: bool,
    pub attempt: u32,
    pub pane: Option<String>,
    pub process: Option<(u32, u64)>,
    pub permissions: Option<String>,
    pub accept_risks: bool,
    pub prompt: Option<String>,
    pub prompt_message_id: Option<String>,
    pub prompt_state: Option<String>,
    pub resumed_pid: Option<(u32, u64)>,
    pub resume_dispatched_at: Option<i64>,
    pub verification: Value,
    pub warnings: Vec<String>,
    pub verified_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}
pub(crate) fn read_session(paths: &Paths, catalog_id: &str) -> Option<SessionRec> {
    serde_json::from_slice(&fs::read(session_file(paths, catalog_id).ok()?).ok()?).ok()
}
pub(crate) fn save_session(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    r.updated_at = now();
    private_dir(&dir(paths))?;
    atomic_write(
        &session_file(paths, &r.session_id)?,
        &serde_json::to_vec_pretty(r)?,
    )
}
pub(crate) fn sessions(paths: &Paths) -> Vec<SessionRec> {
    let mut out: Vec<SessionRec> = fs::read_dir(dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("session-") && n.ends_with(".json")
        })
        .filter_map(|e| serde_json::from_slice::<SessionRec>(&fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    out
}
fn new_rec(s: &Session, run: &str) -> SessionRec {
    let mut ids = s.previous_conversation_ids.clone();
    ids.push(s.conversation_id.clone());
    SessionRec {
        schema_version: 1,
        id: s.id.clone(),
        kind: "root_rekey".into(),
        session_id: s.id.clone(),
        name: s.name.clone(),
        space: s.space.clone(),
        provider: s.provider.clone(),
        account: s.account.clone(),
        conversation_id: s.conversation_id.clone(),
        conversation_ids: ids,
        run: run.into(),
        state: "not_applicable".into(),
        phase: "complete".into(),
        from: s.directory.clone(),
        created_at: now(),
        updated_at: now(),
        ..Default::default()
    }
}

fn read_bases(paths: &Paths) -> keys::Bases {
    fs::read(bases_file(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn save_bases(paths: &Paths, b: &keys::Bases) -> Result<()> {
    private_dir(&dir(paths))?;
    atomic_write(&bases_file(paths), &serde_json::to_vec_pretty(b)?)
}

// -------------------------------------------------------------------- locks

struct Locks {
    _layout: fs::File,
    _daemon: Option<fs::File>,
}
fn lock(paths: &Paths, change: bool) -> Result<Locks> {
    let layout = lock_file(&paths.root.join(layout::LAYOUT_LOCK))?;
    crate::paths::try_lock_brief(&layout)
        .context("LAYOUT_BUSY: another layout or root command is running")?;
    let daemon = if change {
        let daemon = lock_file(&paths.root.join("daemon.lock"))?;
        crate::paths::try_lock_brief(&daemon).context(
            "DAEMON_RUNNING: stop the supervisor (`sessionsation daemon stop`) before `root migrate` or `root rollback`",
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

// -------------------------------------------------------------------- alias

/// Accept a pair proven by a symlink (debug builds only; test fixtures cannot
/// bind-mount). Release builds never honour it.
fn test_symlink_proof() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("SESSIONSATION_TEST_ALIAS_PROOF").is_some_and(|v| v == "symlink")
}
/// Prove `from` and `to` are one tree (§3.1). `Ok(proof)` or `Err(reason)`.
pub(crate) fn prove(from: &Path, to: &Path, dry_run: bool) -> std::result::Result<String, String> {
    let fm = fs::symlink_metadata(from).map_err(|_| "from_missing".to_string())?;
    let tm = fs::symlink_metadata(to).map_err(|_| "to_missing".to_string())?;
    if test_symlink_proof()
        && (fm.file_type().is_symlink() || tm.file_type().is_symlink())
        // raw-path: debug-only test proof (a symlink fake of the bind)
        && fs::canonicalize(from).ok() == fs::canonicalize(to).ok()
    // raw-path: test proof
    {
        return Ok("test_symlink".into());
    }
    if fm.file_type().is_symlink() || tm.file_type().is_symlink() {
        return Err("symlink".into());
    }
    if !fm.is_dir() || !tm.is_dir() {
        return Err("not_a_directory".into());
    }
    // No other array disk or pool may hold content of the same share.
    if let (Some(fp), Some(fname), Some(tname), Some(share)) = (
        from.parent().and_then(Path::parent),
        from.parent().and_then(Path::file_name),
        to.parent().and_then(Path::file_name),
        from.file_name(),
    ) {
        for branch in other_branches(fp, fname, tname) {
            let dir = branch.join(share);
            match stat_with_timeout(&dir) {
                StatResult::Timeout => return Err(format!("stat_timeout:{}", dir.display())),
                StatResult::NonEmptyDir => {
                    return Err(format!("other_branch:{}", dir.display()));
                }
                // Missing, or an empty directory: no divergent content.
                StatResult::Absent | StatResult::Empty => {}
            }
        }
    }
    if fm.dev() == tm.dev() && fm.ino() == tm.ino() {
        return Ok("same_inode".into());
    }
    if dry_run {
        return Err("probe_required".into());
    }
    let probe = format!(".sessionsation-alias-probe-{}", uuid::Uuid::new_v4());
    if fs::write(to.join(&probe), b"sessionsation").is_err() {
        return Err("probe_unwritable".into());
    }
    let same = fs::read(from.join(&probe)).is_ok_and(|b| b == b"sessionsation");
    let _ = fs::remove_file(to.join(&probe));
    if same {
        Ok("probe_file".into())
    } else {
        Err("probe_mismatch".into())
    }
}

/// Branches that could hold another copy of a share: siblings of the alias
/// parent named `disk<N>` (array disks) and siblings that are local block-device
/// mount points (pools). Never `user`, `user0`, `remotes`, `addons`, `rootshare`,
/// `disks` (unassigned devices) or network/FUSE mounts, and never the two
/// spellings themselves. Nothing here is stat-ed: names come from `read_dir` and
/// `/proc/self/mountinfo`.
pub(crate) fn other_branches(
    parent: &Path,
    from_name: &std::ffi::OsStr,
    to_name: &std::ffi::OsStr,
) -> Vec<PathBuf> {
    const SKIP: &[&str] = &["user", "user0", "remotes", "addons", "rootshare", "disks"];
    const REMOTE: &[&str] = &[
        "nfs",
        "nfs4",
        "cifs",
        "smb3",
        "smbfs",
        "fuse",
        "fuse.shfs",
        "fuse.sshfs",
        "fuse.rclone",
        "9p",
        "ceph",
        "glusterfs",
    ];
    let mut pools = std::collections::BTreeSet::new();
    if let Ok(info) = fs::read_to_string("/proc/self/mountinfo") {
        for line in info.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let Some(sep) = fields.iter().position(|f| *f == "-") else {
                continue;
            };
            let (Some(point), Some(fstype)) = (fields.get(4), fields.get(sep + 1)) else {
                continue;
            };
            let point = Path::new(point);
            if point.parent() == Some(parent)
                && !REMOTE.contains(fstype)
                && !fstype.starts_with("fuse")
            {
                pools.insert(point.to_path_buf());
            }
        }
    }
    let mut out = vec![];
    for e in fs::read_dir(parent).into_iter().flatten().flatten() {
        let n = e.file_name();
        let name = n.to_string_lossy();
        if n == from_name || n == to_name || SKIP.contains(&name.as_ref()) {
            continue;
        }
        let disk = name
            .strip_prefix("disk")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
        // raw-path: both from /proc/self/mountinfo and read_dir of one parent
        if disk || pools.contains(&e.path()) {
            out.push(e.path());
        }
    }
    out.sort();
    out
}
pub(crate) enum StatResult {
    Absent,
    Empty,
    NonEmptyDir,
    Timeout,
}
/// `stat` + one `readdir` entry with a 2 s bound (a dead mount never hangs the run).
pub(crate) fn stat_with_timeout(dir: &Path) -> StatResult {
    let (tx, rx) = std::sync::mpsc::channel();
    let d = dir.to_path_buf();
    std::thread::spawn(move || {
        let r = match fs::symlink_metadata(&d) {
            Err(_) => StatResult::Absent,
            Ok(m) if !m.is_dir() => StatResult::NonEmptyDir,
            Ok(_) => match fs::read_dir(&d).map(|mut x| x.next().is_some()) {
                Ok(false) => StatResult::Empty,
                _ => StatResult::NonEmptyDir,
            },
        };
        let _ = tx.send(r);
    });
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or(StatResult::Timeout)
}

/// Every stored absolute path this plan looks at (for the share expansion).
fn stored_paths(paths: &Paths) -> Vec<PathBuf> {
    let mut out = vec![paths.root.clone()];
    if let Ok(list) = catalog::list(paths) {
        out.extend(
            list.iter()
                .filter_map(|s| s.directory.as_ref().map(PathBuf::from)),
        );
    }
    for (dir, field) in REGISTRY_FIELDS {
        for (_, v) in json_files(&paths.root.join(dir)) {
            if let Some(s) = v.pointer(field).and_then(Value::as_str) {
                out.push(s.into());
            }
        }
    }
    if let Ok(c) = crate::store::load_config(paths) {
        out.push(c.codex_binary.into());
    }
    out
}
/// The proposed entries (one per first component under `from` found in stored
/// paths) and, separately, the components that could not be proven.
fn propose(paths: &Paths, from: &Path, to: &Path, dry_run: bool) -> (Vec<Entry>, Vec<Value>) {
    let mut shares = std::collections::BTreeSet::new();
    // The root's own share, also when it is already opened by the new spelling.
    // raw-path: literal spellings define the alias being proposed
    if let Ok(rest) = paths.root.strip_prefix(to)
        && let Some(first) = rest.components().next()
    {
        shares.insert(first.as_os_str().to_owned());
    }
    for p in stored_paths(paths) {
        if let Ok(rest) = p.strip_prefix(from)
            && let Some(first) = rest.components().next()
        {
            shares.insert(first.as_os_str().to_owned());
        }
    }
    let mut proven = vec![];
    let mut unproven = vec![];
    for share in shares {
        let (f, t) = (from.join(&share), to.join(&share));
        match prove(&f, &t, dry_run) {
            Ok(proof) => proven.push(Entry {
                from: f,
                to: t,
                proof,
            }),
            Err(reason) => unproven.push(json!({"from":f,"to":t,"reason":reason})),
        }
    }
    (proven, unproven)
}

// ------------------------------------------------------------ preconditions

/// Non-terminal operations (P4): every `api operations` row that is not terminal.
/// S3 (review 2): every journal file in every operation directory is read
/// directly (read only: nothing is settled or reconciled, so a dry run beside a
/// live supervisor writes nothing here). A journal that cannot be read or has no
/// recognisable phase blocks like an unfinished one (fail closed).
fn blocking_operations(paths: &Paths) -> Vec<Value> {
    const GENERIC: &[&str] = &[
        "complete",
        "completed",
        "failed",
        "cancelled",
        "recovered",
        "abandoned",
        "expired",
        "rejected",
        "done",
        "deleted",
        "removed",
        "restored",
        "archived",
    ];
    let skip = [
        layout::LIFECYCLE_ADMISSION,
        layout::MOVE_BACKUPS,
        layout::ROOT_MIGRATIONS,
    ];
    let mut out = vec![];
    let mut dirs: Vec<&str> = layout::OPERATION_DIRS
        .iter()
        .copied()
        .filter(|d| !skip.contains(d))
        .collect();
    dirs.push(layout::SHARED_MIGRATIONS);
    for d in dirs {
        // Retention's final phases for this directory plus the generic ones: retention
        // lists only what it may prune (launches omit `abandoned`, which is final).
        let mut terminal: Vec<&str> = crate::retention::PRUNABLE
            .iter()
            .find(|(p, _)| *p == d)
            .map(|(_, f)| f.to_vec())
            .unwrap_or_default();
        terminal.extend_from_slice(GENERIC);
        let Ok(entries) = fs::read_dir(paths.root.join(d)) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json")
                || !e.file_type().is_ok_and(|t| t.is_file())
            {
                continue;
            }
            let v: Value = match fs::read(&p).map(|b| serde_json::from_slice(&b)) {
                Ok(Ok(v)) => v,
                _ => {
                    out.push(json!({"file":p,"kind":d,"phase":"unreadable"}));
                    continue;
                }
            };
            let phase = v["phase"].as_str().or_else(|| v["state"].as_str());
            // `deletions/<thread>.json` are permanent deletion tombstones (never pruned,
            // no phase); the deletion operation itself is journaled elsewhere.
            if d == layout::DELETIONS && phase.is_none() {
                continue;
            }
            let done = phase.is_some_and(|ph| {
                terminal.contains(&ph) || (d == layout::SHARED_MIGRATIONS && ph == "incomplete")
            });
            if !done {
                out.push(
                    json!({"id":v["id"].as_str().or_else(|| v["request"]["request_id"].as_str()),
                    "file":p,"kind":d,"phase":phase}),
                );
            }
        }
    }
    // B2: a hosted Claude worker listens on `<runtime>/hosted/<c>.sock`, and the
    // runtime directory follows the root spelling, so activation would orphan it.
    match catalog::list(paths) {
        Ok(list) => {
            for s in list.iter().filter(|s| s.provider == "claude") {
                let mut ids = s.previous_conversation_ids.clone();
                ids.push(s.conversation_id.clone());
                for c in ids {
                    if claude::hosted::live(paths, &c) {
                        out.push(json!({"id":s.id,"kind":"claude_hosted_worker","phase":"live",
                            "detail":"stop the hosted (stream-json) worker first; its socket is in the runtime directory, which moves with the root spelling"}));
                    }
                }
            }
        }
        Err(e) => {
            out.push(json!({"id":null,"kind":"catalog_unreadable","phase":format!("{e:#}")}));
        }
    }
    out
}

fn json_files(dir: &Path) -> Vec<(PathBuf, Value)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut out: Vec<(PathBuf, Value)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let v = serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
            Some((e.path(), v))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Current-state records rewritten by Tier R: (directory, JSON pointer).
const REGISTRY_FIELDS: &[(&str, &str)] = &[
    (layout::CONVERSATIONS, "/owner"),
    (layout::MANAGED_PANES, "/home"),
    (layout::HERDR_PANES, "/binding/home"),
    (layout::THREAD_LOCATIONS, "/cwd"),
];
/// Alias-only classes: never rewritten, only counted (files holding the old prefix).
const ALIAS_ONLY: &[&str] = &[
    "operations",
    "messaging",
    "sessions/archived",
    "sessions/preserved",
    "cache",
    "logs",
];
fn count_holding(dir: &Path, needle: &[u8], skip: &Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if d.starts_with(skip) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t)
                    if t.is_file()
                        && e.metadata().is_ok_and(|m| m.len() <= 8 << 20)
                        && fs::read(e.path())
                            .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle)) =>
                {
                    n += 1;
                }
                _ => {}
            }
        }
    }
    n
}

// --------------------------------------------------------------------- plan

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionPlan {
    pub id: String,
    pub name: String,
    pub space: Option<String>,
    pub provider: String,
    pub account: String,
    /// Planned outcome: rekey | deferred_running | archived_rekey_at_restore |
    /// left_behind | unsupported_provider | not_applicable
    pub state: String,
    pub reason: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub key_from: Option<String>,
    pub key_to: Option<String>,
    pub memory_key: Option<String>,
    pub found: Vec<Found>,
    pub collisions: Vec<String>,
    pub bytes: u64,
    #[serde(skip)]
    pub home: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct CodexPlan {
    pub id: String,
    pub name: String,
    pub thread: String,
    pub account: String,
    pub from: String,
    pub to: String,
    pub running: bool,
    pub saved_location: Option<String>,
    pub state: String,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct KeyPlan {
    pub key_from: String,
    pub key_to: String,
    pub sessions: Vec<String>,
    pub memory: MemoryResult,
    pub target_exists: bool,
    pub target_entries: Vec<String>,
    pub same_name: Vec<String>,
}
#[derive(Default)]
pub(crate) struct Plan {
    pub alias: Alias,
    pub unproven: Vec<Value>,
    pub files: Vec<Value>,
    pub links: Vec<LinkChange>,
    pub claude_json: Vec<Value>,
    pub keys: BTreeMap<(String, String), KeyPlan>,
    pub sessions: Vec<SessionPlan>,
    pub codex: Vec<CodexPlan>,
    pub alias_only: BTreeMap<String, u64>,
    pub unowned: BTreeMap<String, u64>,
    pub superseded: BTreeMap<String, u64>,
    pub codex_untrusted: Vec<String>,
    pub config: Option<Value>,
}

/// Managed Claude homes (`accounts/claude/<name>`, legacy `claude-accounts/<name>`).
fn claude_homes(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for parent in [
        root.join(layout::ACCOUNTS).join("claude"),
        root.join("claude-accounts"),
    ] {
        if let Ok(entries) = fs::read_dir(&parent) {
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir())
                    && crate::paths::validate_name(&e.file_name().to_string_lossy()).is_ok()
                {
                    out.push(e.path());
                }
            }
        }
    }
    out.sort();
    out
}
fn claude_home(paths: &Paths, account: &str) -> Option<PathBuf> {
    native::account(paths, ProviderId::Claude, account)
        .ok()
        .map(|a| a.home)
}
fn archived_ids(paths: &Paths) -> std::collections::BTreeSet<String> {
    crate::external::archiving::metadata(paths)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| v["archived"] == true)
        .map(|(k, _)| k)
        .collect()
}
/// Codex inventory as the supervisor sees it (host panes enriched with their
/// thread, cwd and rollout through the app-servers). `Err` = the probe failed.
pub(crate) type CodexView = std::result::Result<Vec<crate::external::Instance>, String>;
pub(crate) async fn codex_instances(paths: &Paths) -> CodexView {
    crate::external::live_sessions(paths)
        .await
        .map_err(|e| format!("{e:#}"))
}
/// Processes whose command line names one of `ids` (a provider started with
/// `--resume <id>` / `--session-id <id>`). `Err` when `/proc` is unreadable.
pub(crate) fn processes_naming(ids: &[String]) -> std::result::Result<Vec<u32>, String> {
    processes_naming_in(Path::new("/proc"), ids)
}
pub(crate) fn processes_naming_in(
    proc_dir: &Path,
    ids: &[String],
) -> std::result::Result<Vec<u32>, String> {
    let me = std::process::id();
    let entries =
        fs::read_dir(proc_dir).map_err(|e| format!("{} unreadable: {e}", proc_dir.display()))?;
    let mut out = vec![];
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(cmd) = fs::read(e.path().join("cmdline")) else {
            continue;
        };
        if cmd
            .split(|b| *b == 0)
            .any(|a| ids.iter().any(|id| a == id.as_bytes()))
        {
            out.push(pid);
        }
    }
    Ok(out)
}
/// Is this catalog session recognised as running by the code paths the
/// supervisor uses (Claude registry + environment, Codex host inventory)?
/// `Err` when it cannot be decided (a failed probe is never "stopped").
pub(crate) fn recognition(
    paths: &Paths,
    s: &Session,
    codex: &CodexView,
) -> std::result::Result<bool, String> {
    match native::of(&s.provider) {
        Some(ProviderId::Claude) => {
            let home = claude_home(paths, &s.account)
                .ok_or_else(|| format!("account claude:{} unavailable", s.account))?;
            match claude::observe(&home, &s.conversation_id, s.attachment.as_ref()).availability {
                "running" => Ok(true),
                "stopped" | "stale" => {
                    let mut ids = s.previous_conversation_ids.clone();
                    ids.push(s.conversation_id.clone());
                    Ok(!processes_naming(&ids)?.is_empty())
                }
                other => Err(format!("Claude state {other}")),
            }
        }
        Some(p @ (ProviderId::Gemini | ProviderId::Antigravity)) => {
            let a = native::account(paths, p, &s.account).map_err(|e| format!("{e:#}"))?;
            native::live_sessions(p, &a.home)
                .map(|l| l.iter().any(|l| l.session_id == s.conversation_id))
                .map_err(|e| format!("{e:#}"))
        }
        _ => Ok(codex.as_ref().map_err(Clone::clone)?.iter().any(|i| {
            !i.inactive
                && i.thread_id == s.conversation_id
                && crate::external::account_for_home(paths, &i.home) == s.account // raw-path: account names
        })),
    }
}
/// The supervisor's own recognition, with no command-line fallback (review 3,
/// B1): Claude by its registry and environment (`claude::observe`), Codex by the
/// host inventory. The gate must see exactly what the supervisor sees, so a
/// process that merely names the conversation never counts, and anything but
/// `running` or `stopped`/`stale` is a probe error.
pub(crate) fn supervisor_recognition(
    paths: &Paths,
    s: &Session,
    codex: &CodexView,
) -> std::result::Result<bool, String> {
    match native::of(&s.provider) {
        Some(ProviderId::Claude) => {
            let home = claude_home(paths, &s.account)
                .ok_or_else(|| format!("account claude:{} unavailable", s.account))?;
            let o = claude::observe(&home, &s.conversation_id, s.attachment.as_ref());
            match o.availability {
                // Re-check 2: "running" only by the recorded process's argv (no
                // registry holder, no transcript) is not a recognised session: a
                // gate row had a conversation before activation.
                "running"
                    if o.detail
                        .as_deref()
                        .is_some_and(|d| d.starts_with("NO_CONVERSATION_YET")) =>
                {
                    Ok(false)
                }
                "running" => Ok(true),
                "stopped" | "stale" => Ok(false),
                other => Err(format!("Claude state {other}")),
            }
        }
        _ => recognition(paths, s, codex),
    }
}
/// The F2 before-snapshot. `Err` refuses Tier R before anything changes.
pub(crate) async fn gate_rows(paths: &Paths) -> Result<Vec<GateRow>> {
    let codex = codex_instances(paths).await;
    if let Err(e) = &codex {
        bail!("ROOT_MIGRATION_BLOCKED: the Codex inventory for the rollback gate failed: {e}");
    }
    let mut out = vec![];
    // Archived sessions cannot run (review 3, S2).
    let archived = archived_ids(paths);
    for s in catalog::list(paths)?
        .into_iter()
        .filter(|s| !archived.contains(&s.id))
    {
        let r = recognition(paths, &s, &codex);
        if s.attachment.is_none() && r == Ok(false) {
            continue;
        }
        out.push(GateRow {
            session_id: s.id.clone(),
            name: s.name.clone(),
            space: s.space.clone(),
            provider: s.provider.clone(),
            account: s.account.clone(),
            conversation_id: s.conversation_id.clone(),
            pane: s.attachment.as_ref().map(|a| a.target.clone()),
            pid: s.attachment.as_ref().map(|a| a.pid),
            running_before: r.clone().unwrap_or(true),
            probe_error: r.err(),
        });
    }
    let failed: Vec<String> = out
        .iter()
        .filter_map(|g| {
            g.probe_error
                .as_ref()
                .map(|e| format!("{}: {e}", g.session_id))
        })
        .collect();
    ensure!(
        failed.is_empty(),
        "ROOT_MIGRATION_BLOCKED: the running state of these sessions cannot be decided, so the rollback gate would be blind: {}",
        failed.join("; ")
    );
    Ok(out)
}

/// The Codex thread's effective working directory: its saved location, else
/// the cwd its rollout recorded.
fn codex_cwd(paths: &Paths, s: &Session) -> (Option<String>, Option<String>) {
    let saved = crate::external::relocation::saved_location(paths, &s.conversation_id);
    if saved.is_some() {
        return (saved.clone(), saved);
    }
    let home = paths.account(&s.account).ok();
    let rollout = home.and_then(|h| {
        crate::external::rollout_by_thread(&h, &s.conversation_id)
            .ok()
            .flatten()
    });
    let cwd = rollout
        .and_then(|r| crate::external::session_meta(&r).ok())
        .and_then(|m| m["cwd"].as_str().map(str::to_owned));
    (cwd, None)
}

pub(crate) fn build_plan(paths: &Paths, alias: &Alias, codex: &CodexView) -> Result<Plan> {
    let mut plan = Plan {
        alias: alias.clone(),
        ..Default::default()
    };
    // Registry records.
    for (dir, field) in REGISTRY_FIELDS {
        for (file, v) in json_files(&paths.root.join(dir)) {
            if let Some(old) = v.pointer(field).and_then(Value::as_str) {
                let new = alias.logical(Path::new(old));
                if new.as_os_str() != old {
                    // raw-path: `file` comes from listing under paths.root
                    plan.files.push(json!({"file":file.strip_prefix(&paths.root).unwrap_or(&file),"pointer":field,"old":old,"new":new}));
                }
            }
        }
    }
    if let Ok(c) = crate::store::load_config(paths) {
        let old = PathBuf::from(&c.codex_binary);
        let new = alias.logical(&old);
        if new != old {
            let same = fs::metadata(&old)
                .ok()
                .zip(fs::metadata(&new).ok())
                .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino());
            plan.config = Some(json!({"field":"codex_binary","old":old,"new":new,"rewrite":same}));
        }
    }
    // Shared-store links of every managed Claude home.
    let canonical_root = alias.logical(&paths.root);
    for home in claude_homes(&paths.root) {
        for name in crate::shared_state::NAMES {
            let link = home.join(name);
            if !fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
                continue;
            }
            let Ok(old) = fs::read_link(&link) else {
                continue;
            };
            let new = crate::shared_state::target(&canonical_root, name);
            if old != new && alias.logical(&old) == new {
                plan.links.push(LinkChange {
                    home: home.clone(),
                    name: name.into(),
                    old,
                    new,
                });
            }
        }
        if let Ok(text) = fs::read_to_string(home.join(".claude.json"))
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && let Some(projects) = v["projects"].as_object()
        {
            let keys: Vec<String> = projects
                .keys()
                .filter(|k| {
                    let m = alias.logical(Path::new(k)).to_string_lossy().into_owned();
                    m != **k && !projects.contains_key(&m)
                })
                .map(|k| alias.logical(Path::new(k)).to_string_lossy().into_owned())
                .collect();
            if !keys.is_empty() {
                plan.claude_json.push(json!({"home":home,"keys":keys}));
            }
        }
    }
    // Sessions.
    let archived = archived_ids(paths);
    let list = catalog::list(paths)?;
    let mut owned: std::collections::BTreeSet<String> = Default::default();
    for s in &list {
        owned.extend(s.previous_conversation_ids.iter().cloned());
        owned.insert(s.conversation_id.clone());
    }
    let bases = read_bases(paths);
    for s in &list {
        match native::of(&s.provider) {
            Some(ProviderId::Claude) => {
                let p = plan_claude(paths, alias, s, archived.contains(&s.id), codex)?;
                if let (Some(km), Some(kt), Some(home)) = (&p.memory_key, &p.key_to, &p.home)
                    && km != kt
                    && matches!(
                        p.state.as_str(),
                        "rekey" | "deferred_running" | "archived_rekey_at_restore"
                    )
                {
                    let projects = home.join("projects");
                    let entry = plan
                        .keys
                        .entry((km.clone(), kt.clone()))
                        .or_insert_with(|| {
                            let target = projects.join(kt);
                            let mut target_entries: Vec<String> = fs::read_dir(&target)
                                .into_iter()
                                .flatten()
                                .flatten()
                                .map(|e| e.file_name().to_string_lossy().into_owned())
                                .collect();
                            target_entries.sort();
                            let mut src_names = vec![];
                            keys_files(&projects.join(km), &mut src_names);
                            let mut dst_names = vec![];
                            keys_files(&target, &mut dst_names);
                            let same_name = src_names
                                .into_iter()
                                .filter(|n| dst_names.contains(n))
                                .collect();
                            let mut base = bases
                                .0
                                .get(&keys::pair(km, kt))
                                .cloned()
                                .unwrap_or_default();
                            // Dry run: nothing is written, so no backup directory is used.
                            let memory = keys::sync_memory(
                                &projects.join(km).join("memory"),
                                &target.join("memory"),
                                &mut base,
                                Path::new(""),
                                kt,
                                true,
                            )
                            .unwrap_or_else(|e| MemoryResult {
                                // Unreadable memory blocks like a conflict (fail closed).
                                conflicts: vec![format!("<unreadable: {e:#}>")],
                                ..Default::default()
                            });
                            KeyPlan {
                                key_from: km.clone(),
                                key_to: kt.clone(),
                                target_exists: target.is_dir(),
                                target_entries,
                                same_name,
                                memory,
                                ..Default::default()
                            }
                        });
                    entry.sessions.push(s.id.clone());
                }
                plan.sessions.push(p);
            }
            Some(_) => {
                let maps = s
                    .directory
                    .as_deref()
                    .is_some_and(|d| alias.maps(Path::new(d)));
                plan.sessions.push(SessionPlan {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    space: s.space.clone(),
                    provider: s.provider.clone(),
                    account: s.account.clone(),
                    state: if maps {
                        "unsupported_provider"
                    } else {
                        "not_applicable"
                    }
                    .into(),
                    reason: None,
                    from: s.directory.clone(),
                    to: None,
                    key_from: None,
                    key_to: None,
                    memory_key: None,
                    found: vec![],
                    collisions: vec![],
                    bytes: 0,
                    home: None,
                });
            }
            None => {
                if archived.contains(&s.id) {
                    continue;
                }
                let (cwd, saved) = codex_cwd(paths, s);
                let Some(cwd) = cwd else { continue };
                let to = alias.logical(Path::new(&cwd));
                if to.as_os_str() == cwd.as_str() {
                    continue;
                }
                // Only a thread positively known not to run is re-keyed.
                let known = recognition(paths, s, codex);
                let running = known.clone().unwrap_or(true);
                let (state, reason) = match known {
                    Err(e) => ("left_behind", Some(format!("probe_failed: {e}"))),
                    Ok(true) => ("deferred_running", None),
                    Ok(false) if !to.is_dir() => ("left_behind", Some("target_missing".into())),
                    Ok(false) => ("rekey", None),
                };
                plan.codex.push(CodexPlan {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    thread: s.conversation_id.clone(),
                    account: s.account.clone(),
                    from: cwd,
                    to: to.to_string_lossy().into_owned(),
                    running,
                    saved_location: saved,
                    state: state.into(),
                    reason,
                });
            }
        }
    }
    // A memory conflict on a key defers every session of that key.
    for ((_, _), k) in plan.keys.iter() {
        if !k.memory.clean() {
            for p in plan
                .sessions
                .iter_mut()
                .filter(|p| k.sessions.contains(&p.id))
            {
                if p.state == "rekey" {
                    p.state = "left_behind".into();
                    p.reason = Some("memory_conflict".into());
                }
            }
        }
    }
    // Unowned transcripts and `.superseded-*` leftovers per old key.
    let store = list
        .iter()
        .filter(|s| s.provider == "claude")
        .find_map(|s| claude_home(paths, &s.account))
        .map(|h| h.join("projects"));
    if let Some(projects) = store
        && let Ok(dirs) = fs::read_dir(&projects)
    {
        let old_prefixes: Vec<String> = alias
            .entries
            .iter()
            .map(|e| keys::key(&e.from.to_string_lossy()))
            .collect();
        for d in dirs.flatten() {
            let k = d.file_name().to_string_lossy().into_owned();
            if !old_prefixes.iter().any(|p| k.starts_with(p.as_str())) {
                continue;
            }
            for f in fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let n = f.file_name().to_string_lossy().into_owned();
                if n.contains(".superseded-") {
                    *plan.superseded.entry(k.clone()).or_default() += 1;
                } else if let Some(id) = n.strip_suffix(".jsonl")
                    && !owned.contains(id)
                {
                    *plan.unowned.entry(k.clone()).or_default() += 1;
                }
            }
        }
    }
    // Codex directories without a new-spelling trust table (R7).
    for c in &plan.codex {
        let table = format!("[projects.\"{}\"]", c.to);
        let trusted = crate::layout::codex_homes(&paths.root)
            .iter()
            .any(|(_, h)| {
                fs::read_to_string(h.join("config.toml")).is_ok_and(|t| t.contains(&table))
            });
        if !trusted && !plan.codex_untrusted.contains(&c.to) {
            plan.codex_untrusted.push(c.to.clone());
        }
    }
    for class in ALIAS_ONLY {
        let needles: Vec<String> = alias
            .entries
            .iter()
            .map(|e| format!("{}/", e.from.display()))
            .collect();
        let mut n = 0;
        for needle in &needles {
            n += count_holding(
                &paths.root.join(class),
                needle.as_bytes(),
                &paths.root.join(layout::ROOT_MIGRATIONS),
            );
        }
        plan.alias_only.insert((*class).into(), n);
    }
    Ok(plan)
}
fn keys_files(dir: &Path, out: &mut Vec<String>) {
    fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&p, base, out);
            } else {
                out.push(
                    p.strip_prefix(base)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    walk(dir, dir, out);
}

fn plan_claude(
    paths: &Paths,
    alias: &Alias,
    s: &Session,
    archived: bool,
    codex: &CodexView,
) -> Result<SessionPlan> {
    let mut p = SessionPlan {
        id: s.id.clone(),
        name: s.name.clone(),
        space: s.space.clone(),
        provider: s.provider.clone(),
        account: s.account.clone(),
        state: "not_applicable".into(),
        reason: None,
        from: s.directory.clone(),
        to: None,
        key_from: None,
        key_to: None,
        memory_key: None,
        found: vec![],
        collisions: vec![],
        bytes: 0,
        home: claude_home(paths, &s.account),
    };
    let Some(dir) = s.directory.as_deref() else {
        return Ok(p);
    };
    let new_dir = alias.logical(Path::new(dir)).to_string_lossy().into_owned();
    if new_dir == dir {
        return Ok(p);
    }
    p.to = Some(new_dir.clone());
    p.memory_key = Some(keys::key(dir));
    p.key_to = Some(keys::key(&new_dir));
    if archived {
        p.state = "archived_rekey_at_restore".into();
        return Ok(p);
    }
    let Some(home) = p.home.clone() else {
        p.state = "left_behind".into();
        p.reason = Some("account_unavailable".into());
        return Ok(p);
    };
    let mut ids = s.previous_conversation_ids.clone();
    ids.push(s.conversation_id.clone());
    match keys::locate(&home.join("projects"), &ids) {
        Ok(found) => {
            p.key_from = found
                .iter()
                .find(|f| f.conversation_id == s.conversation_id)
                .or(found.first())
                .map(|f| f.key.clone());
            p.bytes = found
                .iter()
                .filter_map(|f| fs::metadata(&f.transcript).ok().map(|m| m.len()))
                .sum();
            p.collisions = keys::collisions(&home.join("projects"), &found, &keys::key(&new_dir));
            p.found = found;
        }
        Err(e) => {
            p.state = "left_behind".into();
            p.reason = Some(format!("{e:#}"));
            return Ok(p);
        }
    }
    // B1: only a session positively known to be stopped is planned `rekey`.
    match recognition(paths, s, codex) {
        Ok(false) => {}
        Ok(true) => {
            p.state = "deferred_running".into();
            return Ok(p);
        }
        Err(e) => {
            p.state = "left_behind".into();
            p.reason = Some(format!("probe_failed: {e}"));
            return Ok(p);
        }
    }
    let (state, reason) = if keys::key(&new_dir).len() > keys::KEY_MAX {
        (
            "left_behind",
            Some("key_too_long (ROOT_REKEY_UNSUPPORTED)".to_string()),
        )
    } else if !Path::new(&new_dir).is_dir() {
        ("left_behind", Some("target_missing".to_string()))
    } else if !p.collisions.is_empty() {
        (
            "left_behind",
            Some("name_collision (ROOT_REKEY_CONFLICT)".to_string()),
        )
    } else {
        ("rekey", None)
    };
    p.state = state.into();
    p.reason = reason;
    Ok(p)
}

fn plan_json(plan: &Plan, paths: &Paths) -> Value {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for p in &plan.sessions {
        *counts
            .entry(format!("{}:{}", p.provider, p.state))
            .or_default() += 1;
    }
    for c in &plan.codex {
        *counts.entry(format!("codex:{}", c.state)).or_default() += 1;
    }
    let mut by_class: BTreeMap<String, u64> = BTreeMap::new();
    for f in &plan.files {
        let file = f["file"].as_str().unwrap_or_default();
        let class = REGISTRY_FIELDS
            .iter()
            .find(|(d, _)| file.starts_with(d))
            .map_or("other", |(d, _)| d);
        *by_class.entry(class.into()).or_default() += 1;
    }
    let claude_rekey: Vec<_> = plan
        .sessions
        .iter()
        .filter(|p| p.state == "rekey")
        .map(|p| p.id.clone())
        .collect();
    let catalog_rows = plan
        .sessions
        .iter()
        .filter(|p| matches!(p.state.as_str(), "rekey" | "archived_rekey_at_restore"))
        .count();
    by_class.insert(
        "registry/session-catalog (directory)".into(),
        catalog_rows as u64,
    );
    by_class.insert("layout.json".into(), 1);
    by_class.insert("shared-store links".into(), plan.links.len() as u64);
    by_class.insert(
        ".claude.json project entries".into(),
        plan.claude_json
            .iter()
            .map(|c| c["keys"].as_array().map_or(0, Vec::len) as u64)
            .sum(),
    );
    let memory_conflicts: Vec<Value> = plan
        .keys
        .values()
        .flat_map(|k| {
            k.memory
                .conflicts
                .iter()
                .map(|f| json!({"key_from":k.key_from,"key_to":k.key_to,"file":f}))
                .collect::<Vec<_>>()
        })
        .collect();
    let collisions: Vec<Value> = plan
        .sessions
        .iter()
        .flat_map(|p| {
            p.collisions
                .iter()
                .map(|c| json!({"session":p.id,"path":c}))
                .collect::<Vec<_>>()
        })
        .collect();
    let left: Vec<&SessionPlan> = plan
        .sessions
        .iter()
        .filter(|p| p.state == "left_behind")
        .collect();
    let memory_split: Vec<String> = plan
        .keys
        .values()
        .filter(|k| {
            let states: Vec<&str> = plan
                .sessions
                .iter()
                .filter(|p| k.sessions.contains(&p.id))
                .map(|p| p.state.as_str())
                .collect();
            states.contains(&"rekey") && states.iter().any(|s| *s != "rekey") || !k.memory.clean()
        })
        .map(|k| k.key_from.clone())
        .collect();
    let existing_targets: Vec<Value> = plan
        .keys
        .values()
        .filter(|k| k.target_exists)
        .map(|k| json!({"key":k.key_to,"from_key":k.key_from,"entries":k.target_entries,"same_name_files":k.same_name}))
        .collect();
    let mut summary = vec![format!(
        "root migrate plan for {}: {} alias entr{} proven, {} unproven share(s)",
        paths.root.display(),
        plan.alias.entries.len(),
        if plan.alias.entries.len() == 1 {
            "y"
        } else {
            "ies"
        },
        plan.unproven.len()
    )];
    summary.push(format!(
        "registry values to rewrite: {} (+{} catalog directories, {} links, {} .claude.json entries)",
        plan.files.len(),
        catalog_rows,
        plan.links.len(),
        by_class[".claude.json project entries"]
    ));
    summary.push(format!(
        "sessions: {}",
        counts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    summary.push(format!(
        "claude keys: {}, existing target keys: {}, memory conflicts: {}, name collisions: {}, left behind: {}",
        plan.keys.len(),
        existing_targets.len(),
        memory_conflicts.len(),
        collisions.len(),
        left.len()
    ));
    summary.push(format!(
        "codex thread locations to write: {} (running, deferred: {})",
        plan.codex.iter().filter(|c| c.state == "rekey").count(),
        plan.codex.iter().filter(|c| c.running).count()
    ));
    json!({
        "entries": plan.alias.entries,
        "unproven": plan.unproven,
        "unmapped": plan.unproven.iter().map(|u| u["from"].clone()).collect::<Vec<_>>(),
        "counts": {"sessions": counts, "rewrite": by_class, "alias_only": plan.alias_only},
        "files_to_rewrite": plan.files,
        "config": plan.config,
        "links": plan.links,
        "claude_json": plan.claude_json,
        "claude": {
            "keys": plan.keys.values().collect::<Vec<_>>(),
            "sessions": plan.sessions.iter().filter(|p| p.provider == "claude").collect::<Vec<_>>(),
            "rekey": claude_rekey,
        },
        "other_providers": plan.sessions.iter().filter(|p| p.provider != "claude").collect::<Vec<_>>(),
        "memory_conflicts": memory_conflicts,
        "collisions": collisions,
        "memory_split": memory_split,
        "unmapped_sessions": plan.sessions.iter().filter(|p| p.provider == "claude" && p.to.is_none() && p.from.as_deref().is_some_and(|d| d.starts_with('/'))).map(|p| json!({"session":p.id,"directory":p.from})).collect::<Vec<_>>(),
        "left_behind": left.iter().map(|p| json!({"session":p.id,"name":p.name,"space":p.space,"reason":p.reason})).collect::<Vec<_>>(),
        "codex": plan.codex,
        "existing_target_keys": existing_targets,
        "unowned_transcripts": plan.unowned,
        "superseded_leftovers": plan.superseded,
        "codex_untrusted_dirs": plan.codex_untrusted,
        "summary": summary,
    })
}

// ---------------------------------------------------------------- Tier R

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub from: PathBuf,
    pub to: PathBuf,
    pub backup: Option<PathBuf>,
    pub dry_run: bool,
    pub allow_full_copy: bool,
}

/// `root migrate --from F --to T --backup DIR [--dry-run] [--allow-full-copy]`.
pub async fn migrate(paths: &Paths, o: &Options) -> Result<Value> {
    ensure!(
        o.from.is_absolute() && o.to.is_absolute() && o.from != o.to,
        "INVALID_ARGUMENT: --from and --to must be two different absolute paths"
    );
    let _locks = lock(paths, !o.dry_run)?;
    let record = layout::read_record(&paths.root)?
        .context("LAYOUT_MIGRATION_REQUIRED: the data root has no layout record; run `sessionsation layout migrate` first")?;
    let unfinished = latest_run(paths).filter(|r| !matches!(r.state.as_str(), "rolled_back"));
    // P1
    let resume = match (&record.root_alias, &unfinished) {
        (Some(a), Some(run)) if a.state == "pending" || a.state == "active" => {
            ensure!(
                run.from == o.from && run.to == o.to,
                "ROOT_MIGRATION_MISMATCH: run {} re-keys {} -> {}; finish or roll it back first",
                run.id,
                run.from.display(),
                run.to.display()
            );
            if a.state == "active" && run.state == "complete" && !o.dry_run {
                return Ok(
                    json!({"already_active":true,"run":run.id,"status":status(paths).await?}),
                );
            }
            Some(run.clone())
        }
        (Some(a), None) if a.state != "inactive" => bail!(
            "ROOT_MIGRATION_MISMATCH: layout.json has a {} root alias without its run journal",
            a.state
        ),
        _ => {
            ensure!(
                record.version == layout::VERSION && record.state == State::Complete,
                "LAYOUT_MIGRATION_REQUIRED: root migrate needs a complete layout version {} root (found version {}, {:?})",
                layout::VERSION,
                record.version,
                record.state
            );
            None
        }
    };
    // P2
    let (entries, unproven) = match &resume {
        Some(run) => (run.entries.clone(), vec![]),
        None => propose(paths, &o.from, &o.to, o.dry_run),
    };
    let alias = Alias::new(entries.clone());
    // The root is covered when it is under a proven pair in either spelling
    // (opened by the old one, or already canonical).
    let root_entry = alias.entry_for(&paths.root).cloned().or_else(|| {
        alias
            .entries
            .iter()
            // raw-path: the root already in the literal new spelling
            .find(|e| paths.root.starts_with(&e.to))
            .cloned()
    });
    let p2 = root_entry.is_some();
    // P4
    let blocked = if resume.is_none() {
        blocking_operations(paths)
    } else {
        vec![]
    };
    // P6
    let backup_state = match (&resume, &o.backup) {
        (Some(run), _) => Ok(run.backup.clone()),
        (None, None) => Err(
            "BACKUP_REQUIRED: pass --backup DIR (a new or empty directory outside the data root)"
                .to_string(),
        ),
        (None, Some(b)) => check_backup(paths, &alias, b).map(|_| b.clone()),
    };
    if resume
        .as_ref()
        .is_some_and(|r| step_index(&r.step) >= step_index("mark"))
    {
        // An interrupted run past `mark`: compare through the alias while planning.
        alias::install(&entries);
    }
    // S2 (review 2): opened by the new spelling, the probes (plan and gate
    // snapshot) would not match processes that use the old one: compare through
    // the proposed entries from the start.
    if alias.entry_for(&paths.root).is_none() && !entries.is_empty() {
        alias::install(&entries);
    }
    let codex = codex_instances(paths).await;
    let plan = build_plan(paths, &alias, &codex)?;
    if o.dry_run {
        let mut v = plan_json(&plan, paths);
        v["dry_run"] = json!(true);
        v["preconditions"] = json!({
            "P1_layout": {"ok": true, "layout_version": record.version, "resume_run": resume.as_ref().map(|r| r.id.clone())},
            "P2_alias_proven": {"ok": p2, "root_entry": root_entry, "unproven": unproven},
            "P3_supervisor_stopped": {"ok": daemon_stopped(paths), "checked_by":"daemon.lock (dry run does not hold it)"},
            "P4_no_operations_in_flight": {"ok": blocked.is_empty(), "blocked": blocked},
            "P5_disk_space": {"checked_by":"real run only"},
            "P6_backup": {"ok": backup_state.is_ok(), "detail": backup_state.as_ref().err(),
                "reflink": if o.allow_full_copy {"full copies allowed"} else {"probed by the real run (refused without --allow-full-copy when unsupported)"}},
            "codex_inventory": {"ok": codex.is_ok(), "detail": codex.as_ref().err()},
        });
        // Review 3 S2: the before-snapshot the real run takes (it refuses on any
        // probe error), evaluated here so a refusal shows up days before the window.
        v["preconditions"]["gate_snapshot"] = match gate_rows(paths).await {
            Ok(rows) => json!({"ok": true, "sessions": rows.len(),
                "running": rows.iter().filter(|r| r.running_before).count()}),
            Err(e) => json!({"ok": false, "detail": format!("{e:#}")}),
        };
        return Ok(v);
    }
    ensure!(
        p2,
        "ROOT_ALIAS_UNPROVEN: the data root {} is not under a proven {} -> {} pair ({})",
        paths.root.display(),
        o.from.display(),
        o.to.display(),
        serde_json::to_string(&unproven).unwrap_or_default()
    );
    ensure!(
        blocked.is_empty(),
        "ROOT_MIGRATION_BLOCKED: operations are in flight; recover or abandon them first: {}",
        serde_json::to_string(&blocked).unwrap_or_default()
    );
    let backup = backup_state.map_err(|e| anyhow::anyhow!(e))?;
    // P6 (backup quality): without --allow-full-copy the backup must reflink.
    if resume.is_none() && !o.allow_full_copy {
        probe_reflink(paths, &backup)?;
    }
    let mut run = match resume {
        Some(r) => r,
        None => {
            let layout_bytes = fs::read(paths.root.join(layout::LAYOUT_FILE))?;
            let mut r = Run {
                schema_version: 1,
                id: uuid::Uuid::new_v4().to_string(),
                kind: "root_migrate".into(),
                from: o.from.clone(),
                to: o.to.clone(),
                backup: backup.clone(),
                allow_full_copy: o.allow_full_copy,
                entries: entries.clone(),
                unmapped: unproven
                    .iter()
                    .filter_map(|u| u["from"].as_str().map(str::to_owned))
                    .collect(),
                step: "admitted".into(),
                state: "running".into(),
                detail: "Admitted.".into(),
                root_id: record
                    .root_alias
                    .as_ref()
                    .map(|a| a.root_id.clone())
                    .filter(|i| !i.is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                canonical_root: alias.logical(&paths.root),
                previous_root: paths.root.clone(),
                layout_before: serde_json::to_value(&record)?,
                layout_before_sha256: sha(&layout_bytes),
                gate_before: gate_rows(paths).await?,
                created_at: now(),
                ..Default::default()
            };
            save_run(paths, &mut r)?;
            r
        }
    };
    drive(paths, &mut run, &plan)?;
    Ok(
        json!({"run":run.id,"step":run.step,"state":run.state,"verify":run.verify,"status":status(paths).await?}),
    )
}

/// Reflink a small file from the data root into the backup directory (the
/// same pair of file systems the real copies use). Refused when unsupported.
fn probe_reflink(paths: &Paths, backup: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::io::AsRawFd;
    let src_dir = dir(paths);
    private_dir(&src_dir)?;
    if fs::symlink_metadata(backup).is_err() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(backup)?;
    }
    let id = uuid::Uuid::new_v4();
    let a = src_dir.join(format!(".reflink-probe-{id}"));
    let b = backup.join(format!(".reflink-probe-{id}"));
    fs::write(&a, vec![7u8; 8192])?;
    let result = (|| -> std::io::Result<()> {
        let src = fs::File::open(&a)?;
        let dst = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&b)?;
        const FICLONE: libc::c_ulong = 0x4004_9409;
        if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })();
    // Our own probe files only.
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    result.map_err(|e| {
        anyhow::anyhow!(
            "BACKUP_REFLINK_UNSUPPORTED: {} cannot reflink from the data root ({e}); the backup would be a full copy. Repeat with --allow-full-copy to accept that",
            backup.display()
        )
    })
}

fn daemon_stopped(paths: &Paths) -> bool {
    lock_file(&paths.root.join("daemon.lock"))
        .ok()
        .is_some_and(|f| crate::paths::try_lock_brief(&f).is_ok())
}
fn check_backup(paths: &Paths, a: &Alias, b: &Path) -> std::result::Result<(), String> {
    if !b.is_absolute() {
        return Err("BACKUP_REQUIRED: --backup must be an absolute path".into());
    }
    // Every spelling of both (as given, resolved, through the proposed alias in
    // either direction): the process-wide alias is not installed yet.
    let forms = |p: &Path| {
        let resolved = crate::layout::resolve_path(p);
        let mut v = vec![p.to_path_buf(), resolved.clone()];
        for x in [p.to_path_buf(), resolved] {
            v.push(a.logical(&x));
            v.push(a.physical_old(&x));
        }
        v
    };
    let (bs, rs) = (forms(b), forms(&paths.root));
    if bs
        .iter()
        .any(|x| rs.iter().any(|r| x.starts_with(r) || r.starts_with(x)))
    {
        return Err("BACKUP_REQUIRED: --backup must be outside the data root".into());
    }
    if fs::symlink_metadata(b).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("BACKUP_REQUIRED: --backup must not be a symlink".into());
    }
    if b.exists() && fs::read_dir(b).map_or(true, |mut d| d.next().is_some()) {
        return Err(format!(
            "BACKUP_NOT_EMPTY: {} already has content; choose a new or empty directory",
            b.display()
        ));
    }
    Ok(())
}

fn drive(paths: &Paths, run: &mut Run, plan: &Plan) -> Result<()> {
    let alias = Alias::new(run.entries.clone());
    if step_index(&run.step) < step_index("backup") {
        backup(paths, run)?;
        step(paths, run, "backup", "Backup taken and verified.")?;
    }
    if step_index(&run.step) < step_index("mark") {
        let mut record = layout::read_record(&paths.root)?.unwrap_or_default();
        record.version = layout::ALIAS_VERSION;
        record.state = State::Migrating;
        record.root_alias = Some(RootAlias {
            version: 1,
            state: "pending".into(),
            root_id: run.root_id.clone(),
            canonical_root: run.canonical_root.clone(),
            previous_root: run.previous_root.clone(),
            entries: run.entries.clone(),
            normalize_requests: true,
            run: run.id.clone(),
            backup: run.backup.clone(),
            activated_at: None,
        });
        layout::write_record(&paths.root, &record)?;
        step(
            paths,
            run,
            "mark",
            "Layout version 3 (migrating); other commands fail closed until activation.",
        )?;
    }
    // From here on comparisons in this process use the alias.
    alias::install(&run.entries);
    if step_index(&run.step) < step_index("links") {
        relink(paths, run, plan)?;
        step(
            paths,
            run,
            "links",
            "Shared-store links point at the canonical root.",
        )?;
    }
    if step_index(&run.step) < step_index("registry") {
        rewrite_registry(paths, run, &alias)?;
        step(paths, run, "registry", "Registry records rewritten.")?;
    }
    if step_index(&run.step) < step_index("claude") {
        claude_step(paths, run, &alias, plan)?;
        step(
            paths,
            run,
            "claude",
            "Claude project entries, memory and stopped sessions re-keyed.",
        )?;
    }
    if step_index(&run.step) < step_index("codex") {
        codex_step(paths, run, plan)?;
        step(
            paths,
            run,
            "codex",
            "Codex thread locations recorded for threads that are not running.",
        )?;
    }
    if step_index(&run.step) < step_index("activate") {
        let mut record = layout::read_record(&paths.root)?.unwrap_or_default();
        let at = now();
        if let Some(a) = record.root_alias.as_mut() {
            a.state = "active".into();
            a.activated_at = Some(at);
        }
        record.version = layout::ALIAS_VERSION;
        record.state = State::Complete;
        record
            .history
            .push(json!({"at":at,"action":"root_migrate","run":run.id,"entries":run.entries}));
        layout::write_record(&paths.root, &record)?;
        run.activated_at = Some(at);
        // B2 (review 2): clients that derive the control socket from the old
        // spelling (Agent-DM) reach the canonical runtime through a symlink.
        // (Checked again by `verify`.)
        let _ = runtime::ensure(paths);
        step(paths, run, "activate", "Root alias active.")?;
    }
    // Verification runs again on every rerun until the run completes (a death
    // after the `verify` save, or a fix forward after `verify_failed`).
    if step_index(&run.step) < step_index("complete") {
        run.verify = verify(paths, run);
        run.state = if run.verify.is_empty() {
            "complete".into()
        } else {
            "verify_failed".into()
        };
        let detail = if run.verify.is_empty() {
            "Verified.".to_string()
        } else {
            format!(
                "ROOT_VERIFY_FAILED: {} item(s); the alias stays active (fix forward or `root rollback`)",
                run.verify.len()
            )
        };
        step(paths, run, "verify", &detail)?;
        if run.verify.is_empty() {
            step(paths, run, "complete", "Root migration complete.")?;
        }
    }
    Ok(())
}

fn backup(paths: &Paths, run: &Run) -> Result<()> {
    let dir = &run.backup;
    if fs::symlink_metadata(dir).is_err() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("BACKUP_REQUIRED: cannot create {}", dir.display()))?;
    }
    use std::os::unix::fs::DirBuilderExt;
    let root = &paths.root;
    let skip = |name: &str| {
        matches!(
            name,
            "accounts"
                | "shared"
                | "daemon.lock"
                | "layout.lock"
                | "claude-accounts"
                | "gemini-accounts"
                | "antigravity-accounts"
        ) || name.starts_with("registry.db")
    };
    let mut manifest = vec![];
    for entry in fs::read_dir(root)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if skip(&name) {
            continue;
        }
        let target = dir.join("root").join(&name);
        if fs::symlink_metadata(&target).is_ok() {
            continue;
        }
        if let Some(p) = target.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(p)?;
        }
        // Review 3 S1: copied under a temporary name and renamed only when
        // complete, so a rerun after a crash never takes a partial copy for done
        // (an interrupted `.partial-*` copy is left in the backup, never counted).
        let partial = dir
            .join("root")
            .join(format!(".partial-{name}-{}", uuid::Uuid::new_v4()));
        let status = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto", "--"])
            .arg(entry.path())
            .arg(&partial)
            .status()
            .context("BACKUP_FAILED: cp is unavailable")?;
        ensure!(status.success(), "BACKUP_FAILED: copying {name}");
        #[cfg(test)]
        tests::die_check(&format!("backup:{name}"));
        fs::rename(&partial, &target)?;
    }
    // SQLite databases: consistent copies.
    for db in [layout::REGISTRY_DB, layout::RECEIPTS_DB] {
        let from = root.join(db);
        if !fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let to = dir.join("root").join(format!("{db}.vacuum"));
        if fs::symlink_metadata(&to).is_ok() {
            continue;
        }
        if let Some(p) = to.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(p)?;
        }
        let c = rusqlite::Connection::open_with_flags(
            &from,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        c.busy_timeout(std::time::Duration::from_secs(10))?;
        let partial = to.with_file_name(format!(".partial-vacuum-{}", uuid::Uuid::new_v4()));
        c.execute("VACUUM INTO ?1", [partial.to_string_lossy()])
            .with_context(|| format!("BACKUP_FAILED: {db}"))?;
        fs::rename(&partial, &to)?;
    }
    // The shared Claude projects store (reflink; full copy only when allowed).
    let projects = crate::shared_state::target(root, "projects");
    let shared_copy = dir.join("shared-claude-projects");
    if projects.is_dir() && fs::symlink_metadata(&shared_copy).is_err() {
        let mut stats = crate::shared_state::migrate::CopyStats::default();
        let partial = dir.join(format!(
            ".partial-shared-claude-projects-{}",
            uuid::Uuid::new_v4()
        ));
        crate::shared_state::migrate::copy_tree(
            &projects,
            &partial,
            run.allow_full_copy,
            &mut stats,
        )?;
        #[cfg(test)]
        tests::die_check("backup:shared");
        fs::rename(&partial, &shared_copy)?;
        manifest.push(json!({"class":"shared/claude/projects","files":stats.files,"bytes":stats.bytes,"full_copies":stats.full_copies}));
    }
    // Every managed Claude home's `.claude.json` and link targets.
    let mut homes = vec![];
    let mut claude_json_files = vec![];
    for home in claude_homes(root) {
        let name = home
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let f = home.join(".claude.json");
        if f.is_file() {
            let to = dir.join("claude-json").join(format!("{name}.claude.json"));
            // Recheck: re-taken (verified, renamed) when missing or not the
            // source's exact bytes (Claude rewrites the file while it runs).
            let current = keys::sha256_file(&f)?;
            if keys::sha256_file(&to).ok() != Some(current) {
                copy_verified(&f, &to, true)?;
                fs::set_permissions(&to, fs::Permissions::from_mode(0o600))?;
            }
            let (bytes, sha256) = keys::sha256_file(&to)?;
            claude_json_files
                .push(json!({"file":to,"bytes":bytes,"sha256":sha256,"check":"exact"}));
        }
        let links: BTreeMap<String, Option<PathBuf>> = crate::shared_state::NAMES
            .iter()
            .map(|n| ((*n).to_owned(), fs::read_link(home.join(n)).ok()))
            .collect();
        homes.push(json!({"home":home,"links":links}));
    }
    // Verification. First the source side (review 3 S1): every file of every
    // copied entry has a copy of the same size (a transcript may have grown);
    // a missing or wrong-sized copy is taken again through `copy_verified`.
    // Then the copy side: every copy's sha256 against its source (exact; a
    // byte prefix only for transcripts that grew), re-taken once on mismatch.
    let mut files = claude_json_files;
    let entries: Vec<String> = fs::read_dir(root)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !skip(n))
        .collect();
    for name in entries {
        verify_complete(
            &root.join(&name),
            &dir.join("root").join(&name),
            Path::new(&name),
            run.allow_full_copy,
        )?;
    }
    if projects.is_dir() {
        verify_complete(&projects, &shared_copy, Path::new(""), run.allow_full_copy)?;
    }
    verify_tree(&dir.join("root"), root, run.allow_full_copy, &mut files)?;
    if shared_copy.is_dir() {
        verify_tree(&shared_copy, &projects, run.allow_full_copy, &mut files)?;
    }
    // Re-check 2: the copies are durable before the manifest is written and the
    // step is journaled done (a host crash must not leave a "done" backup of
    // unflushed copies). The root's file system too: reflinked extents.
    syncfs(dir)?;
    syncfs(root)?;
    let total = files.len();
    let doc = json!({"version":1,"run":run.id,"root":root,"created_at":now(),"classes":manifest,"homes":homes,
        "file_count":total,"files":files});
    atomic_write(&dir.join("root-backup.json"), &serde_json::to_vec(&doc)?)?;
    Ok(())
}
/// `syncfs(2)` the file system holding `path`.
fn syncfs(path: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = fs::File::open(path)?;
    let rc = unsafe { libc::syncfs(f.as_raw_fd()) };
    ensure!(
        rc == 0,
        "BACKUP_FAILED: syncfs {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}
/// The one way a single backup file reaches its final name: copied to a
/// `.partial-` sibling, checked against the source (same size and sha256; a
/// growing transcript may differ only by having grown, then its copied bytes
/// must be the source's prefix), then renamed over the final name. A copy error
/// or a mismatch fails and leaves nothing at the final name.
fn copy_verified(src: &Path, dst: &Path, allow_full: bool) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = dst.parent().context("BACKUP_FAILED: no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let partial = parent.join(format!(
        ".partial-{}-{}",
        dst.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let mut stats = crate::shared_state::migrate::CopyStats::default();
    crate::shared_state::migrate::copy_tree(src, &partial, allow_full, &mut stats)
        .with_context(|| format!("BACKUP_FAILED: copying {}", src.display()))?;
    let (bytes, digest) = keys::sha256_file(&partial)?;
    let (src_bytes, src_digest) = keys::sha256_file(src)?;
    let grows = src.extension().is_some_and(|x| x == "jsonl");
    let same = (bytes == src_bytes && digest == src_digest)
        || (grows
            && bytes < src_bytes
            && keys::sha256_prefix(src, bytes)?.as_deref() == Some(digest.as_str()));
    ensure!(
        same,
        "BACKUP_VERIFY_FAILED: the copy of {} does not match its source (size {bytes} vs {src_bytes}); the partial copy {} was kept",
        src.display(),
        partial.display()
    );
    fs::rename(&partial, dst)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Walk the SOURCE: every regular file has a copy whose size is at most the
/// source's (a live file can only have grown since; `verify_tree` checks the
/// bytes). A missing copy is taken once more; still missing, or larger than its
/// source, fails with `BACKUP_VERIFY_FAILED`. The file counts are compared too.
/// Regenerable logs/cache, SQLite files (their consistent copies are `.vacuum`),
/// this tool's own journals and atomic-write temp files are not required.
fn verify_complete(source: &Path, copy: &Path, rel: &Path, allow_full: bool) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let exempt = |rel: &Path| {
        let name = rel.to_string_lossy();
        rel.starts_with("logs")
            || rel.starts_with("cache")
            || rel.starts_with(layout::ROOT_MIGRATIONS)
            || rel
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".tmp"))
            || [".db", ".db-wal", ".db-shm", ".db-journal", ".lock"]
                .iter()
                .any(|s| name.ends_with(s))
    };
    let mut failures = vec![];
    let (mut sources, mut copies) = (0u64, 0u64);
    let mut stack = vec![(source.to_path_buf(), copy.to_path_buf(), rel.to_path_buf())];
    while let Some((s, c, r)) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&s) else {
            continue; // gone since the listing: verify_tree reports a copy without source
        };
        if m.is_dir() {
            for e in fs::read_dir(&s)?.flatten() {
                stack.push((e.path(), c.join(e.file_name()), r.join(e.file_name())));
            }
            continue;
        }
        if !m.is_file() || exempt(&r) {
            continue;
        }
        sources += 1;
        // Same size, or (a transcript only) a copy its source has grown from.
        let grows = r.extension().is_some_and(|x| x == "jsonl");
        let fits = |c: &Path| {
            fs::symlink_metadata(c).is_ok_and(|cm| {
                cm.is_file()
                    && fs::metadata(&s)
                        .is_ok_and(|x| cm.len() == x.len() || (grows && cm.len() < x.len()))
            })
        };
        if !fits(&c) {
            // Missing or wrong-sized: taken again through the verified writer.
            // A copy error fails the backup and leaves nothing at the final name.
            let _ = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(c.parent().unwrap_or(copy));
            if let Err(e) = copy_verified(&s, &c, allow_full) {
                failures.push(format!("{} ({e:#})", r.display()));
                continue;
            }
        }
        if fits(&c) {
            copies += 1;
        } else {
            failures.push(format!("{} (size differs from its source)", r.display()));
        }
    }
    ensure!(
        failures.is_empty() && sources == copies,
        "BACKUP_VERIFY_FAILED: {} of {} source file(s) are not in the backup: {}",
        sources - copies,
        sources,
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}
/// Hash every file of the copy and compare it with its source: exact for quiet
/// files, byte prefix for files that grew (live transcripts). A file that
/// changed otherwise is copied once more and checked again; a file that still
/// differs, or whose source is gone, fails the backup (`BACKUP_VERIFY_FAILED`).
/// Regenerable logs/cache and the database copies are labelled, not compared.
fn verify_tree(copy: &Path, source: &Path, allow_full: bool, out: &mut Vec<Value>) -> Result<()> {
    fn classify(p: &Path, src: &Path, rel: &Path) -> Result<(u64, String, &'static str)> {
        let (bytes, digest) = keys::sha256_file(p)?;
        let regenerable = rel.starts_with("logs") || rel.starts_with("cache");
        // The consistent database copies are the `.vacuum` files; the raw copies
        // of SQLite files and their sidecars change with every connection.
        let name = rel.to_string_lossy();
        let database = rel.extension().is_some_and(|x| x == "vacuum")
            || [".db", ".db-wal", ".db-shm", ".db-journal"]
                .iter()
                .any(|s| name.ends_with(s));
        let check = match keys::sha256_file(src) {
            _ if database => "database",
            Ok((_, d)) if d == digest => "exact",
            // A prefix is accepted only for a transcript that grew (recheck item 2);
            // any other shorter copy, an empty one included, is a changed copy.
            Ok(_) if rel.extension().is_some_and(|x| x == "jsonl") => {
                match keys::sha256_prefix(src, bytes)? {
                    Some(d) if d == digest => "prefix",
                    _ => "changed",
                }
            }
            Ok(_) if regenerable => "changed_regenerable",
            Ok(_) => "changed",
            Err(_) if regenerable => "gone_regenerable",
            Err(_) => "source_gone",
        };
        Ok((bytes, digest, check))
    }
    let mut failures = vec![];
    let mut stack = vec![copy.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)?.flatten() {
            let t = e.file_type()?;
            let p = e.path();
            if t.is_dir() {
                stack.push(p);
                continue;
            }
            if !t.is_file() {
                continue;
            }
            let rel = p.strip_prefix(copy).unwrap_or(&p).to_path_buf();
            // Interrupted copies of an earlier attempt (kept, never counted).
            if rel
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with(".partial-"))
            {
                continue;
            }
            let src = source.join(&rel);
            let (mut bytes, mut digest, mut check) = classify(&p, &src, &rel)?;
            if check == "changed" {
                // Our own copy differs from its source: take it again (verified,
                // then renamed over the final name; a failure keeps the old copy
                // and fails the backup below).
                match copy_verified(&src, &p, allow_full) {
                    Ok(()) => (bytes, digest, check) = classify(&p, &src, &rel)?,
                    Err(e) => failures.push(format!("{} ({e:#})", rel.display())),
                }
            }
            if matches!(check, "changed" | "source_gone") {
                failures.push(format!("{} ({check})", rel.display()));
            }
            out.push(json!({"file":rel,"bytes":bytes,"sha256":digest,"check":check}));
        }
    }
    ensure!(
        failures.is_empty(),
        "BACKUP_VERIFY_FAILED: {} file(s) of the backup do not match their source: {}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn relink(paths: &Paths, run: &mut Run, plan: &Plan) -> Result<()> {
    for l in &plan.links {
        let link = l.home.join(&l.name);
        let current = fs::read_link(&link).ok();
        if current.as_deref() == Some(l.new.as_path()) {
            // raw-path: journal identity of a planned link
            if !run
                .links
                .iter()
                // raw-path: journal identity of a planned link
                .any(|x| x.home == l.home && x.name == l.name)
            {
                run.links.push(l.clone());
            }
            continue;
        }
        ensure!(
            current.as_deref() == Some(l.old.as_path()),
            "ROOT_VERIFY_FAILED: link {} changed since the plan",
            link.display()
        );
        // raw-path: journal identity of a planned link
        if !run
            .links
            .iter()
            // raw-path: journal identity of a planned link
            .any(|x| x.home == l.home && x.name == l.name)
        {
            run.links.push(l.clone());
            save_run(paths, run)?;
        }
        repoint(&link, &l.new)?;
    }
    Ok(())
}
/// Atomically replace the symlink `link` by one to `target` (rename(2) over it).
fn repoint(link: &Path, target: &Path) -> Result<()> {
    let tmp = link.with_file_name(format!(
        ".{}.rootmig-{}",
        link.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    std::os::unix::fs::symlink(target, &tmp)?;
    if let Err(e) = fs::rename(&tmp, link) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("SHARED_STATE_LINK_FAILED: {}", link.display()));
    }
    if let Some(p) = link.parent() {
        fs::File::open(p)?.sync_all()?;
    }
    Ok(())
}

fn pretty(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|w| w == b"\n  ")
}
fn rewrite_json(
    paths: &Paths,
    run: &Run,
    file: &Path,
    pointer: &str,
    new_value: impl Fn(&str) -> Option<String>,
) -> Result<bool> {
    let bytes = fs::read(file)?;
    let mut value: Value = serde_json::from_slice(&bytes)?;
    let Some(old) = value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(false);
    };
    let Some(new) = new_value(&old).filter(|n| *n != old) else {
        return Ok(false);
    };
    *value.pointer_mut(pointer).expect("checked") = json!(new);
    let mut out = if pretty(&bytes) {
        serde_json::to_vec_pretty(&value)?
    } else {
        serde_json::to_vec(&value)?
    };
    if bytes.ends_with(b"\n") && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    let c = Change {
        file: file
            // raw-path: `file` comes from listing under paths.root
            .strip_prefix(&paths.root)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned(),
        pointer: pointer.into(),
        old: json!(old),
        new: json!(new),
        before_sha256: sha(&bytes),
        after_sha256: sha(&out),
    };
    // Journaled before the write: a crash after the write is always recoverable.
    // Rollback remaps go to their own log, so a rerun never undoes them.
    if run.state == "rolling_back" || run.state.starts_with("rollback") {
        let path = run_file(paths, &run.id)?.with_extension("rollback.jsonl");
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        let mut line = serde_json::to_vec(&c)?;
        line.push(b'\n');
        f.write_all(&line)?;
        f.sync_all()?;
    } else {
        log_change(paths, &run.id, &c)?;
    }
    atomic_write(file, &out)?;
    Ok(true)
}

fn rewrite_registry(paths: &Paths, run: &Run, alias: &Alias) -> Result<()> {
    let map = |old: &str| {
        let p = Path::new(old);
        p.is_absolute()
            .then(|| alias.logical(p).to_string_lossy().into_owned())
    };
    for (dir, field) in REGISTRY_FIELDS {
        for (file, _) in json_files(&paths.root.join(dir)) {
            rewrite_json(paths, run, &file, field, map)?;
        }
    }
    let config = paths.root.join("config.json");
    if config.is_file() {
        rewrite_json(paths, run, &config, "/codex_binary", |old| {
            let old_p = Path::new(old);
            let new = alias.logical(old_p);
            let same = fs::metadata(old_p)
                .ok()
                .zip(fs::metadata(&new).ok())
                .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino());
            same.then(|| new.to_string_lossy().into_owned())
        })?;
    }
    Ok(())
}

fn claude_step(paths: &Paths, run: &mut Run, alias: &Alias, plan: &Plan) -> Result<()> {
    // `.claude.json` project entries (old entries stay).
    for home in claude_homes(&paths.root) {
        // raw-path: journal identity of a home
        if run.claude_json.iter().any(|c| c.home == home) {
            // Journaled by an interrupted run: finish writing (idempotent).
            keys::clone_project_entries(&home, alias, None, false)?;
            continue;
        }
        // S8: the planned clones are journaled before they are written, so a
        // crash in between still lets rollback remove them (matched by value).
        let planned = keys::clone_project_entries(&home, alias, None, true)?;
        run.claude_json.push(ClaudeJson {
            home: home.clone(),
            added: planned,
        });
        save_run(paths, run)?;
        keys::clone_project_entries(&home, alias, None, false)?;
    }
    // Memory: every key pair, before any session moves.
    let mut bases = read_bases(paths);
    let mut conflicted = std::collections::BTreeSet::new();
    let store = plan
        .sessions
        .iter()
        .find_map(|p| p.home.clone())
        .map(|h| h.join("projects"));
    if let Some(projects) = &store {
        for ((km, kt), _) in plan.keys.iter() {
            let base = bases.0.entry(keys::pair(km, kt)).or_default();
            let r = keys::sync_memory(
                &projects.join(km).join("memory"),
                &projects.join(kt).join("memory"),
                base,
                &run.backup,
                kt,
                false,
            )?;
            if !r.clean() {
                conflicted.insert(km.clone());
            }
        }
        save_bases(paths, &bases)?;
    }
    run.memory_conflict_keys = conflicted.iter().cloned().collect();
    save_run(paths, run)?;
    for p in plan.sessions.iter().filter(|p| p.provider == "claude") {
        let Ok(s) = catalog::find(paths, &p.id) else {
            continue;
        };
        let existing = read_session(paths, &s.id);
        // B3(e): an interrupted re-key is reconciled from its own journal first
        // (the plan may already see the new directory and call it not_applicable).
        if let Some(mut rec) = existing.clone()
            && rec.state == "rekeying"
            && rec.phase != "complete"
            && rec.process.is_none()
        {
            let mut ctx = session::Ctx::new(paths, &run.backup, run.allow_full_copy);
            if let Err(e) = session::rekey_stopped(&mut ctx, &mut rec, &s) {
                session::abandon_rekey(paths, &mut rec, &e)?;
            }
            continue;
        }
        if existing.as_ref().is_some_and(|r| {
            (matches!(r.state.as_str(), "rekeyed" | "verified") && r.phase == "complete")
                || !r.moved.is_empty()
                || (r.phase != "complete" && r.phase != "failed")
        }) {
            // Done, files still moved by an earlier attempt, or owned by a Tier S worker.
            continue;
        }
        let mut rec = existing.unwrap_or_else(|| new_rec(&s, &run.id));
        rec.home = p.home.clone();
        rec.to = p.to.clone();
        rec.key_from = p.key_from.clone();
        rec.key_to = p.key_to.clone();
        rec.memory_key = p.memory_key.clone();
        rec.collisions = p.collisions.clone();
        match p.state.as_str() {
            "rekey"
                if p.memory_key
                    .as_ref()
                    .is_some_and(|k| conflicted.contains(k)) =>
            {
                rec.state = "left_behind".into();
                rec.reason = Some("memory_conflict".into());
            }
            "rekey" => {
                rec.state = "rekeying".into();
                rec.phase = "admitted".into();
                save_session(paths, &mut rec)?;
                let mut ctx = session::Ctx::new(paths, &run.backup, run.allow_full_copy);
                if let Err(e) = session::rekey_stopped(&mut ctx, &mut rec, &s) {
                    // B3(b): moved names go back before the session is reported left behind.
                    session::abandon_rekey(paths, &mut rec, &e)?;
                }
                continue;
            }
            "archived_rekey_at_restore" => {
                if let Some(to) = &p.to {
                    let (before, after) = catalog::rekey_directory(
                        paths,
                        &s.id,
                        &s.conversation_id,
                        s.directory.as_deref(),
                        to,
                    )?;
                    rec.revision_before.get_or_insert(before);
                    rec.revision_after = Some(after.revision);
                }
                rec.state = "archived_rekey_at_restore".into();
            }
            other => {
                rec.state = other.into();
                rec.reason = p.reason.clone();
            }
        }
        rec.phase = "complete".into();
        if rec.state != "not_applicable" {
            save_session(paths, &mut rec)?;
        }
    }
    Ok(())
}

fn codex_step(paths: &Paths, run: &Run, plan: &Plan) -> Result<()> {
    // S1: a thread interrupted after its location was journaled (the plan of the
    // rerun no longer lists it: its cwd already maps) is finished from the journal.
    for mut rec in sessions(paths)
        .into_iter()
        .filter(|r| r.provider == "codex" && r.state == "rekeying" && r.codex_location_written)
    {
        session::codex_rekey_stopped(paths, &mut rec)?;
        rec.state = "rekeyed".into();
        rec.phase = "complete".into();
        save_session(paths, &mut rec)?;
    }
    for c in &plan.codex {
        let Ok(s) = catalog::find(paths, &c.id) else {
            continue;
        };
        let mut rec = read_session(paths, &s.id).unwrap_or_else(|| new_rec(&s, &run.id));
        if matches!(rec.state.as_str(), "rekeyed" | "verified") {
            continue;
        }
        rec.from = Some(c.from.clone());
        rec.to = Some(c.to.clone());
        rec.reason = c.reason.clone();
        rec.state = match c.state.as_str() {
            "rekey" => {
                rec.state = "rekeying".into();
                rec.phase = "admitted".into();
                session::codex_rekey_stopped(paths, &mut rec)?;
                "rekeyed".into()
            }
            other => other.into(),
        };
        rec.phase = "complete".into();
        save_session(paths, &mut rec)?;
    }
    Ok(())
}

fn verify(paths: &Paths, run: &Run) -> Vec<String> {
    let mut out: Vec<String> = runtime::ensure(paths)
        .iter()
        .filter(|l| {
            l["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("refused"))
        })
        .map(|l| format!("runtime link: {l}"))
        .collect();
    // Both spellings open as one root.
    let a = Paths::open_any_layout(Some(run.previous_root.clone()));
    let b = Paths::open_any_layout(Some(run.canonical_root.clone()));
    match (a, b) {
        (Ok(a), Ok(b)) if a.root == b.root && a.runtime == b.runtime => {} // raw-path: one root
        (a, b) => out.push(format!(
            "root: the two spellings do not open one root ({:?} / {:?})",
            a.map(|p| p.root),
            b.map(|p| p.root)
        )),
    }
    if let Err(e) = catalog::list(paths) {
        out.push(format!("catalog: {e:#}"));
    }
    let db = paths.root.join(layout::REGISTRY_DB);
    if db.is_file()
        && rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| {
                c.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                    r.get::<_, i64>(0)
                })
            })
            .is_err()
    {
        out.push("registry: registry.db is unreadable".into());
    }
    if crate::shared_state::enabled(&paths.root) {
        for home in claude_homes(&paths.root) {
            if crate::shared_state::any_linked(&home) && !crate::shared_state::all_linked(&home) {
                out.push(format!("links: {} is not fully linked", home.display()));
            }
            for name in crate::shared_state::NAMES {
                if let Ok(t) = fs::read_link(home.join(name))
                    && Alias::new(run.entries.clone()).maps(&t)
                {
                    out.push(format!(
                        "links: {}/{name} still names {}",
                        home.display(),
                        t.display()
                    ));
                }
            }
        }
    }
    let alias = Alias::new(run.entries.clone());
    for (dir, field) in REGISTRY_FIELDS {
        for (file, v) in json_files(&paths.root.join(dir)) {
            if let Some(s) = v.pointer(field).and_then(Value::as_str)
                && alias.maps(Path::new(s))
            {
                out.push(format!("registry: {} {field} still {s}", file.display()));
            }
        }
    }
    for rec in sessions(paths)
        .iter()
        .filter(|r| r.state == "rekeyed" && r.provider == "claude")
    {
        for m in &rec.moved {
            let ok = if fs::symlink_metadata(&m.to).is_ok_and(|x| x.is_dir()) {
                crate::external::archiving::native::tree_digest(&m.to)
                    .is_ok_and(|(_, d)| d == m.digest)
            } else {
                keys::sha256_file(&m.to).is_ok_and(|(_, d)| d == m.digest)
            };
            if !ok {
                out.push(format!(
                    "session {}: {} does not match its digest",
                    rec.session_id,
                    m.to.display()
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- status

/// `root status` (§7.3) plus the F2 rollback gate.
pub async fn status(paths: &Paths) -> Result<Value> {
    let record = layout::read_record(&paths.root)?;
    let run = latest_run(paths);
    // Codex Tier S jobs settle here as well.
    let mut recs = sessions(paths);
    for r in recs.iter_mut().filter(|r| r.state == "relocating") {
        let _ = session::refresh_codex(paths, r).await;
    }
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for r in &recs {
        *counts.entry(r.state.clone()).or_default() += 1;
    }
    let row = |r: &SessionRec| {
        json!({"id":r.session_id,"name":r.name,"space":r.space,"provider":r.provider,"state":r.state,
            "reason":r.reason,"phase":r.phase,"from":r.from,"to":r.to,"key_from":r.key_from,"key_to":r.key_to,
            "bytes":r.retained.iter().filter(|x| !x.dir).map(|x| x.bytes).sum::<u64>(),
            "retained":r.retained.iter().map(|x| json!({"name":x.name,"copy":x.copy,"bytes":x.bytes,"sha256":x.sha256,"verified":x.verified})).collect::<Vec<_>>(),
            "retained_dir":r.retained_dir,"verified_at":r.verified_at,"verification":r.verification,"warnings":r.warnings,
            "codex_job":r.codex_job,"prompt_state":r.prompt_state})
    };
    let retained_bytes: u64 = recs
        .iter()
        .flat_map(|r| r.retained.iter().map(|x| x.bytes))
        .sum();
    let memory_split: Vec<String> = {
        let mut by_key: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for r in recs.iter().filter(|r| r.provider == "claude") {
            if let Some(k) = &r.memory_key {
                by_key.entry(k.clone()).or_default().push(r.state.as_str());
            }
        }
        by_key
            .into_iter()
            .filter(|(_, s)| {
                s.iter().any(|x| matches!(*x, "rekeyed" | "verified"))
                    && s.iter().any(|x| {
                        !matches!(*x, "rekeyed" | "verified" | "archived_rekey_at_restore")
                    })
            })
            .map(|(k, _)| k)
            .chain(run.iter().flat_map(|r| r.memory_conflict_keys.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let memory_conflicts: Vec<Value> = recs
        .iter()
        .filter_map(|r| r.memory.as_ref().map(|m| (r, m)))
        .flat_map(|(r, m)| {
            m.conflicts
                .iter()
                .map(|f| json!({"session":r.session_id,"key_from":r.memory_key,"key_to":r.key_to,"file":f}))
                .collect::<Vec<_>>()
        })
        .collect();
    let collisions: Vec<Value> = recs
        .iter()
        .flat_map(|r| {
            r.collisions
                .iter()
                .map(|c| json!({"session":r.session_id,"path":c}))
                .collect::<Vec<_>>()
        })
        .collect();
    let gate = gate(paths, run.as_ref()).await;
    Ok(json!({
        "layout_version": record.as_ref().map_or(1, |r| r.version),
        "requested_root": alias::requested_root(&paths.root),
        "root": paths.root,
        "root_alias": record.as_ref().and_then(|r| r.root_alias.clone()),
        "run": run.as_ref().map(|r| json!({"id":r.id,"step":r.step,"state":r.state,"detail":r.detail,"from":r.from,"to":r.to,"backup":r.backup,"verify":r.verify,"activated_at":r.activated_at})),
        "counts": counts,
        "sessions": recs.iter().map(row).collect::<Vec<_>>(),
        "deferred_running": recs.iter().filter(|r| r.state == "deferred_running").map(row).collect::<Vec<_>>(),
        "left_behind": recs.iter().filter(|r| matches!(r.state.as_str(), "left_behind" | "failed" | "verify_failed" | "uncertain")).map(row).collect::<Vec<_>>(),
        "memory_split": memory_split,
        "memory_conflicts": memory_conflicts,
        "collisions": collisions,
        "unmapped": run.as_ref().map(|r| r.unmapped.clone()).unwrap_or_default(),
        "retained": {"dir": run.as_ref().map(|r| r.backup.join("sessions")), "bytes": retained_bytes, "deleted_by_finalize": false},
        "gate": gate["gate"],
        "gate_detail": gate,
    }))
}

/// F2 rollback gate: every session the catalog showed attached/running before
/// activation must still be recognised as running now. `pass` | `fail` |
/// `not_applicable` (no activated run).
pub async fn gate(paths: &Paths, run: Option<&Run>) -> Value {
    let Some(run) = run.filter(|r| r.activated_at.is_some() && r.state != "rolled_back") else {
        return json!({"gate":"not_applicable","sessions":[]});
    };
    let codex = codex_instances(paths).await;
    let rows: Vec<Value> = run
        .gate_before
        .iter()
        .map(|g| {
            // A probe that cannot decide counts as "not recognised" (fail closed).
            let probe = catalog::find(paths, &g.session_id)
                .map_err(|e| format!("{e:#}"))
                .and_then(|s| supervisor_recognition(paths, &s, &codex));
            let now = probe.clone().unwrap_or(false);
            let before_known = g.probe_error.is_none();
            let rec = read_session(paths, &g.session_id);
            // A session this tool stopped on purpose is judged by its own outcome.
            // Only a record of THIS run exempts a session (never a leftover of an
            // earlier, rolled-back run).
            let in_tier_s = rec.as_ref().is_some_and(|r| {
                r.run == run.id
                    && matches!(
                        r.phase.as_str(),
                        "stopping"
                            | "stopped"
                            | "retained"
                            | "moved"
                            | "switched"
                            | "resume_pending"
                            | "resuming"
                            | "verifying"
                            | "reverting"
                            | "rollback_admitted"
                            | "rollback_stopping"
                            | "rollback_stopped"
                    )
            });
            // Any probe error after activation fails the row, whatever it was before
            // and whether or not this run stopped it.
            let now_known = probe.is_ok();
            json!({"session_id":g.session_id,"name":g.name,"space":g.space,"provider":g.provider,
                "running_before":g.running_before,"running_now":now,"in_tier_s":in_tier_s,
                "probe_error_before":g.probe_error,"probe_error_now":probe.err(),
                "ok": before_known && now_known && (!g.running_before || now || in_tier_s)})
        })
        .collect();
    let pass = codex.is_ok() && rows.iter().all(|r| r["ok"] == true);
    json!({"gate": if pass {"pass"} else {"fail"}, "run": run.id, "sessions": rows,
        "codex_inventory_error": codex.as_ref().err(),
        "rule":"every session recognised as running before activation is still recognised as running, and every probe succeeded; fail means a missed comparison site or a blind probe: run `root rollback`"})
}

// --------------------------------------------------------------- rollback

/// `root rollback [--dry-run]`: the whole root back to layout version 2 and the
/// original records (supervisor stopped). Sessions still `rekeyed` (never
/// resumed) are moved back first; `verified` sessions stay on their new cwd.
pub fn rollback(paths: &Paths, dry_run: bool) -> Result<Value> {
    let _locks = lock(paths, !dry_run)?;
    let mut run =
        latest_run(paths).context("ROOT_MIGRATION_MISMATCH: no root migration run to roll back")?;
    if run.state == "rolled_back" {
        return Ok(json!({"rolled_back":true,"run":run.id,"already":true}));
    }
    let changes = read_changes(paths, &run.id);
    let recs = sessions(paths);
    // B3(d): every session this run changed and that is not on its new path for good.
    let run_id = run.id.clone();
    let undo = move |r: &SessionRec| {
        r.run == run_id
            && r.state != "verified"
            && (!r.moved.is_empty()
                || r.revision_after.is_some()
                || r.codex_location_written
                // B1 (review 2): an interrupted re-key may have renamed names the
                // journal did not record yet; undo checks the retained digests.
                || (r.phase != "complete" && !r.retained.is_empty()))
    };
    let workers: Vec<String> = recs
        .iter()
        .filter(|r| r.phase != "complete" && r.phase != "failed" && r.phase != "uncertain")
        .filter(|r| !session::worker_gone_pub(paths, r))
        .map(|r| r.session_id.clone())
        .collect();
    let new_spelling = new_spelling_processes(paths, &run);
    let plan = json!({
        "run": run.id,
        "changes": changes.len(),
        "links": run.links.len(),
        "claude_json": run.claude_json.iter().map(|c| c.added.len()).sum::<usize>(),
        "sessions_moved_back": recs.iter().filter(|r| undo(r)).map(|r| r.session_id.clone()).collect::<Vec<_>>(),
        "sessions_left_on_new_path": recs.iter().filter(|r| r.state == "verified").map(|r| r.session_id.clone()).collect::<Vec<_>>(),
        "workers_running": workers,
        "new_spelling_processes": new_spelling,
        "boot_files": "revert the boot files, launcher and /usr/bin link that name the new spelling together with this rollback",
    });
    if dry_run {
        return Ok(json!({"dry_run":true,"plan":plan}));
    }
    ensure!(
        workers.is_empty(),
        "SESSION_OPERATION_IN_PROGRESS: root re-key worker(s) still run for {}; wait for them",
        workers.join(", ")
    );
    // After a rollback the alias is gone, so a process that uses the new spelling
    // of an account home would no longer be recognised as running (F2 in reverse).
    ensure!(
        new_spelling.is_empty(),
        "ROOT_ROLLBACK_BLOCKED: these processes use the new spelling of a home under the data root and would not be recognised after the rollback; stop them first (they resume on the old path afterwards), and revert the boot files with the rollback: {}",
        serde_json::to_string(&new_spelling).unwrap_or_default()
    );
    alias::install(&run.entries);
    let mut record = layout::read_record(&paths.root)?.unwrap_or_default();
    record.state = State::RollingBack;
    layout::write_record(&paths.root, &record)?;
    run.state = "rolling_back".into();
    save_run(paths, &mut run)?;
    let mut problems = vec![];
    // Sessions first (they may use the alias for comparisons).
    for mut r in recs.into_iter().filter(|r| undo(r)) {
        if let Err(e) = session::rollback_stopped(paths, &mut r) {
            problems.push(format!("session {}: {e:#}", r.session_id));
        }
    }
    // Registry values, newest first.
    for c in changes.iter().rev() {
        if let Err(e) = undo_change(paths, &run, c) {
            problems.push(format!("{}: {e:#}", c.file));
        }
    }
    for l in run.links.iter().rev() {
        let link = l.home.join(&l.name);
        if fs::read_link(&link).ok().as_deref() == Some(l.new.as_path())
            && let Err(e) = repoint(&link, &l.old)
        {
            problems.push(format!("link {}: {e:#}", link.display()));
        }
    }
    for c in &run.claude_json {
        if let Err(e) = keys::remove_project_entries(&c.home, &c.added) {
            problems.push(format!("{}: {e:#}", c.home.display()));
        }
    }
    // S6: records written after activation in the new spelling map back.
    let remapped = match remap_after_activation(paths, &run, &changes) {
        Ok(n) => n,
        Err(e) => {
            problems.push(format!("registry remap: {e:#}"));
            0
        }
    };
    if !problems.is_empty() {
        // S5: a partial rollback keeps the alias active (sessions may still be on the
        // new path) and can be run again.
        let mut record = layout::read_record(&paths.root)?.unwrap_or_default();
        record.state = State::Complete;
        layout::write_record(&paths.root, &record)?;
        run.state = "rollback_incomplete".into();
        run.verify = problems.clone();
        save_run(paths, &mut run)?;
        return Ok(
            json!({"rolled_back":false,"state":"rollback_incomplete","run":run.id,"plan":plan,"problems":problems,
                "detail":"fix the problems and run `root rollback` again; the alias stays active meanwhile"}),
        );
    }
    let runtime_links = runtime::remove(&run);
    // layout.json: the original bytes when the backup holds them, else the record.
    let original = run.backup.join("root").join(layout::LAYOUT_FILE);
    match fs::read(&original) {
        Ok(bytes) if sha(&bytes) == run.layout_before_sha256 => {
            atomic_write(&paths.root.join(layout::LAYOUT_FILE), &bytes)?
        }
        _ => {
            let before: layout::Record = serde_json::from_value(run.layout_before.clone())?;
            layout::write_record(&paths.root, &before)?;
        }
    }
    alias::uninstall(&run.entries);
    run.state = "rolled_back".into();
    run.verify = problems.clone();
    step(
        paths,
        &mut run,
        "complete",
        "Rolled back to layout version 2.",
    )?;
    Ok(
        json!({"rolled_back":true,"run":run.id,"plan":plan,"problems":problems,"layout_version":layout::VERSION,
            "remapped_after_activation":remapped,"runtime_links_removed":runtime_links}),
    )
}
/// Records of the rewritten classes that changed after activation and still
/// name the new spelling (written by the supervisor meanwhile) map back through
/// the alias, so they match the old spelling again. Values this run rewrote are
/// handled by the change log; files untouched since activation keep their value.
fn remap_after_activation(paths: &Paths, run: &Run, changes: &[Change]) -> Result<usize> {
    let Some(at) = run.activated_at else {
        return Ok(0);
    };
    let a = Alias::new(run.entries.clone());
    let logged: std::collections::BTreeSet<&str> =
        changes.iter().map(|c| c.file.as_str()).collect();
    let mut n = 0;
    for (dir, field) in REGISTRY_FIELDS {
        for (file, _) in json_files(&paths.root.join(dir)) {
            let rel = file
                .strip_prefix(&paths.root) // raw-path: listed under paths.root
                .unwrap_or(&file)
                .to_string_lossy()
                .into_owned();
            let modified = fs::metadata(&file)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            if modified < at || logged.contains(rel.as_str()) {
                continue;
            }
            let back = |old: &str| {
                let p = Path::new(old);
                let mapped = a.physical_old(p);
                // raw-path: only values that the alias maps change
                (mapped.as_path() != p).then(|| mapped.to_string_lossy().into_owned())
            };
            if rewrite_json(paths, run, &file, field, back)? {
                n += 1;
            }
        }
    }
    Ok(n)
}
/// Processes whose `CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`GEMINI_CLI_HOME` names the
/// new spelling of a path under the data root (read from `/proc/*/environ`).
fn new_spelling_processes(paths: &Paths, run: &Run) -> Vec<Value> {
    let a = Alias::new(run.entries.clone());
    let new_root = a.logical(&paths.root);
    let old_root = a.physical_old(&paths.root);
    // raw-path: the two spellings of the alias are literally different
    if new_root == old_root {
        return vec![];
    }
    let me = std::process::id();
    let mut out = vec![];
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(env) = fs::read(e.path().join("environ")) else {
            continue;
        };
        for var in env.split(|b| *b == 0) {
            let var = String::from_utf8_lossy(var);
            if let Some((k, v)) = var.split_once('=')
                && matches!(k, "CLAUDE_CONFIG_DIR" | "CODEX_HOME" | "GEMINI_CLI_HOME")
                // raw-path: looking for the literal new spelling
                && Path::new(v).starts_with(&new_root)
            // raw-path: the literal new spelling
            {
                out.push(json!({"pid":pid,"variable":k,"value":v}));
            }
        }
    }
    out
}
fn undo_change(paths: &Paths, run: &Run, c: &Change) -> Result<()> {
    let file = paths.root.join(&c.file);
    let Ok(bytes) = fs::read(&file) else {
        return Ok(());
    };
    if sha(&bytes) == c.before_sha256 {
        return Ok(()); // already original
    }
    if sha(&bytes) == c.after_sha256 {
        let original = run.backup.join("root").join(&c.file);
        if let Ok(b) = fs::read(&original)
            && sha(&b) == c.before_sha256
        {
            return atomic_write(&file, &b);
        }
    }
    let mut value: Value = serde_json::from_slice(&bytes)?;
    if value.pointer(&c.pointer) == Some(&c.new) {
        *value.pointer_mut(&c.pointer).expect("checked") = c.old.clone();
        let mut out = if pretty(&bytes) {
            serde_json::to_vec_pretty(&value)?
        } else {
            serde_json::to_vec(&value)?
        };
        if bytes.ends_with(b"\n") && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        atomic_write(&file, &out)?;
    }
    Ok(())
}

// --------------------------------------------------------------- finalize

/// `root finalize [--dry-run] [--accept-unresumed]`: removes old project-key
/// directories that are empty (`rmdir` only). Retained copies under the backup
/// directory are never deleted in 1.54; old `memory/` directories are only
/// reported (`memory_removable`) when every file is identical in the new key.
pub fn finalize(paths: &Paths, dry_run: bool, accept_unresumed: bool) -> Result<Value> {
    let recs = sessions(paths);
    let mut removed = vec![];
    let mut kept = vec![];
    let mut memory_removable = vec![];
    let mut keys_seen = std::collections::BTreeSet::new();
    for r in recs.iter().filter(|r| r.provider == "claude") {
        let done = r.state == "verified" || (accept_unresumed && r.state == "rekeyed");
        let Some(home) = claude_home(paths, &r.account) else {
            continue;
        };
        let projects = home.join("projects");
        for m in &r.moved {
            if let Some(old_key) = m.from.parent()
                && keys_seen.insert(old_key.to_path_buf())
            {
                if !done {
                    kept.push(json!({"dir":old_key,"reason":format!("session {} is {}", r.session_id, r.state)}));
                    continue;
                }
                let empty = fs::read_dir(old_key).is_ok_and(|mut d| d.next().is_none());
                if empty {
                    if !dry_run {
                        fs::remove_dir(old_key)?;
                    }
                    removed.push(old_key.to_path_buf());
                } else {
                    kept.push(json!({"dir":old_key,"reason":"not_empty"}));
                }
            }
        }
        if let (Some(km), Some(kt)) = (&r.memory_key, &r.key_to)
            && km != kt
            && done
        {
            let src = projects.join(km).join("memory");
            let dst = projects.join(kt).join("memory");
            let mut base = Default::default();
            if let Ok(m) = keys::sync_memory(&src, &dst, &mut base, Path::new(""), kt, true)
                && src.is_dir()
                && m.copied.is_empty()
                && m.fast_forwarded.is_empty()
                && m.conflicts.is_empty()
                && m.kept_newer.is_empty()
                && !memory_removable.contains(&json!(src))
            {
                memory_removable.push(json!(src));
            }
        }
    }
    Ok(
        json!({"dry_run":dry_run,"removed_empty_key_dirs":removed,"kept":kept,
        "memory_removable":memory_removable,"memory_removed":false,
        "retained_copies":"kept: retained copies under the backup directory are never deleted by any 1.54 command, including finalize",
        "retained": recs.iter().flat_map(|r| r.retained.iter().map(|x| x.copy.clone())).collect::<Vec<_>>()}),
    )
}

// ------------------------------------------------------------- api/capability

/// Rows for `api operations` (`kind: root_rekey`).
pub fn operations(paths: &Paths) -> Vec<Value> {
    sessions(paths)
        .iter()
        .filter(|r| r.phase != "complete" || r.state == "uncertain")
        .map(|r| {
            json!({"id":r.session_id,"kind":"root_rekey","provider":r.provider,"session_id":r.session_id,
                "phase":r.phase,"state":r.state,"detail":r.detail,"terminal":r.phase == "complete" || r.phase == "failed",
                "uncertain":r.phase == "uncertain","created_at":r.created_at,"updated_at":r.updated_at,
                "next": if r.phase == "uncertain" {"root migrate --session S to continue, or root rollback --session S"} else {"in progress"}})
        })
        .collect()
}

pub fn capability() -> Value {
    let (state, canonical, entries) = alias_state();
    json!({"since_contract":"1.54.0","version":1,"layout_version":layout::ALIAS_VERSION,
        "state":state,"canonical_root":canonical,"entries":entries,
        "commands":["root status","root migrate","root rollback","root finalize"],
        "session_states":SESSION_STATES,"normalize_requests":true,
        "gate":{"command":"root status","field":"gate","values":["pass","fail","not_applicable"]},
        "retained_copies":{"location":"<backup>/sessions/<session>/<attempt>/<key>/","deleted_by":"never (1.54)"},
        "memory_merge":"per_file_fast_forward_conflict_blocks_never_picks_a_winner",
        "errors":ERRORS})
}
fn alias_state() -> (String, Option<PathBuf>, Vec<Entry>) {
    let entries = alias::current().entries;
    (
        if entries.is_empty() {
            "inactive"
        } else {
            "active"
        }
        .into(),
        None,
        entries,
    )
}
