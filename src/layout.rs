//! Data-root layout, version 2.
//!
//! ```text
//! DATA_ROOT/
//!   layout.json        layout version, migration state and record (this module)
//!   config.json        operator settings (unchanged)
//!   daemon.lock        supervisor singleton lock (unchanged: also excludes older builds)
//!   registry/          registry.db, catalogs, session access, pane bindings, provider state
//!   sessions/          archived/, preserved/ (deleted accounts' history), hosted/ (hosted Claude)
//!   operations/        operation journals: launches, resumptions, deletions, moves, ...
//!   messaging/         receipts.db, checkpoints, observations, Claude/Gemini journals, questions
//!   cache/             regenerable data: state snapshots, batch observations, reports, transcript facts
//!   logs/              daemon.log, handoff.log
//!   accounts/<platform>/<name>   account homes (codex, claude, gemini, antigravity)
//! ```
//!
//! Version 1 (the legacy layout) kept every store at the top level and account
//! homes in `accounts/<name>` (Codex), `claude-accounts/`, `gemini-accounts/`
//! and `antigravity-accounts/`. A legacy root is never used by this build until
//! `sessionsation layout migrate` converts it (see [`migrate`]). An account home
//! that was in use during the migration stays at its legacy path ("deferred")
//! and [`account_home`] resolves it there until a later migrate run moves it.
use crate::{paths::private_dir, provider::ProviderId};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub mod migrate;

/// The version a new root is stamped with (and every root before a root alias).
pub const VERSION: u32 = 2;
/// Version 3 (contract 1.54.0): version 2 plus an active root alias (see
/// [`crate::alias`] and `sessionsation root migrate`). Builds that cannot
/// compare paths through the alias refuse it (`LAYOUT_UNSUPPORTED`).
pub const ALIAS_VERSION: u32 = 3;
/// The newest layout this build admits.
pub const MAX_VERSION: u32 = ALIAS_VERSION;
pub const LAYOUT_FILE: &str = "layout.json";
pub const LAYOUT_LOCK: &str = "layout.lock";

pub const REGISTRY: &str = "registry";
pub const SESSIONS: &str = "sessions";
pub const OPERATIONS: &str = "operations";
pub const MESSAGING: &str = "messaging";
pub const CACHE: &str = "cache";
pub const LOGS: &str = "logs";
pub const ACCOUNTS: &str = "accounts";

pub const REGISTRY_DB: &str = "registry/registry.db";
pub const SESSION_ACCESS: &str = "registry/session-access.json";
pub const SESSION_ACCESS_LOCK: &str = "registry/session-access.lock";
pub const ACCOUNT_CATALOG: &str = "registry/account-catalog";
pub const SESSION_CATALOG: &str = "registry/session-catalog";
pub const CONVERSATIONS: &str = "registry/conversations";
pub const THREAD_LOCATIONS: &str = "registry/thread-locations";
pub const MANAGED_PANES: &str = "registry/managed-panes";
pub const HERDR_PANES: &str = "registry/herdr-panes";
pub const PROVIDER_STATE: &str = "registry/provider-state";
/// Per-session auto-recover settings (contract 1.35.0), outside the catalog so
/// pane bookkeeping never changes a session generation.
pub const AUTO_RECOVER: &str = "registry/auto-recover.json";
pub const AUTO_RECOVER_LOCK: &str = "registry/auto-recover.lock";
pub const AUTO_RECOVER_TICK_LOCK: &str = "registry/auto-recover-tick.lock";
/// Per-session turn-end notification settings and event journal (contract 1.38.0).
pub const TURN_NOTIFY: &str = "registry/turn-notify.json";
pub const TURN_NOTIFY_LOCK: &str = "registry/turn-notify.lock";
/// Session events in one shape for every provider (contract 1.64.0): the bounded journal,
/// its cursor/tracker state and their lock.
pub const EVENTS: &str = "registry/events.jsonl";
pub const EVENTS_STATE: &str = "registry/events-state.json";
pub const EVENTS_LOCK: &str = "registry/events.lock";
/// Short-lived claims on idle HERDR panes picked for reuse (contract 1.52.0), so two
/// concurrent starts never type into the same pane. Regenerable; entries expire.
pub const PANE_CLAIMS: &str = "registry/pane-claims.json";
pub const PANE_CLAIMS_LOCK: &str = "registry/pane-claims.lock";

pub const ARCHIVED_SESSIONS: &str = "sessions/archived";
pub const PRESERVED_CONVERSATIONS: &str = "sessions/preserved";
pub const CLAUDE_HOSTED: &str = "sessions/hosted";

pub const LAUNCHES: &str = "operations/launches";
pub const RESUMPTIONS: &str = "operations/resumptions";
pub const DELETIONS: &str = "operations/deletions";
pub const ACCOUNT_DELETIONS: &str = "operations/account-deletions";
pub const ACCOUNT_RENAMES: &str = "operations/account-renames";
/// Codex rate-limit reset credit redemptions (contract 1.53.0); never pruned.
pub const ACCOUNT_RESET_CREDITS: &str = "operations/account-reset-credits";
pub const CLAUDE_SWITCHES: &str = "operations/claude-switches";
/// Native Claude idle-notice subscriptions (contract 1.40.0).
pub const CLAUDE_IDLE_NOTIFY: &str = "operations/claude-idle-notify";
/// Journals of `shared migrate` (contract 1.39.0).
pub const SHARED_MIGRATIONS: &str = "operations/shared-migrations";
pub const AUTO_RECOVERIES: &str = "operations/auto-recoveries";
pub const CONVERSATION_OPERATIONS: &str = "operations/conversation-operations";
pub const HANDOFF_JOBS: &str = "operations/handoff-jobs";
pub const SPACE_MOVES: &str = "operations/space-moves";
pub const LIFECYCLE_ADMISSION: &str = "operations/lifecycle-admission";
pub const MESSAGE_RECOVERIES: &str = "operations/message-recoveries";
/// Codex compactions queued for the next idle point (contract 1.47.0).
pub const COMPACTIONS: &str = "operations/compactions";
pub const COMPACTIONS_LOCK: &str = "operations/compactions.lock";
/// Model changes of running sessions (contract 1.49.0).
pub const MODEL_CHANGES: &str = "operations/model-changes";
pub const MODEL_CHANGES_LOCK: &str = "operations/model-changes.lock";
/// One-pass session launches: brief, verification, delivery (contract 1.49.0).
pub const SESSION_LAUNCHES: &str = "operations/session-launches";
pub const MOVE_BACKUPS: &str = "operations/move-backups";
/// `root migrate` run and per-session journals (contract 1.54.0); never pruned.
pub const ROOT_MIGRATIONS: &str = "operations/root-migrations";
pub const PROJECT_RELOCATION_LOCK: &str = "operations/project-relocation.lock";

pub const RECEIPTS_DB: &str = "messaging/receipts.db";
pub const CLAUDE_STEER: &str = "messaging/claude-steer";
pub const CLAUDE_DELIVERIES: &str = "messaging/claude-deliveries";
pub const QUESTIONS: &str = "messaging/questions";
pub const INPUT_HEALTH: &str = "messaging/input-health";

pub const OBSERVATIONS: &str = "cache/observations";
pub const COMPATIBILITY_REPORT: &str = "cache/compatibility-report.json";
/// Per-transcript facts of Claude transcripts (regenerable; contract 1.57.0).
pub const TRANSCRIPT_FACTS: &str = "cache/claude-transcript-facts.json";

pub const DAEMON_LOG: &str = "logs/daemon.log";
pub const HANDOFF_LOG: &str = "logs/handoff.log";

/// `cache/state-snapshot-<name>.json` (regenerated by `state`).
pub fn state_snapshot(root: &Path, name: &str) -> PathBuf {
    root.join(CACHE).join(format!("state-snapshot-{name}.json"))
}

/// Operation journal directories (retention and admission scans).
pub const OPERATION_DIRS: &[&str] = &[
    LAUNCHES,
    RESUMPTIONS,
    DELETIONS,
    ACCOUNT_DELETIONS,
    ACCOUNT_RENAMES,
    ACCOUNT_RESET_CREDITS,
    CLAUDE_SWITCHES,
    AUTO_RECOVERIES,
    CONVERSATION_OPERATIONS,
    HANDOFF_JOBS,
    SPACE_MOVES,
    LIFECYCLE_ADMISSION,
    MESSAGE_RECOVERIES,
    COMPACTIONS,
    MODEL_CHANGES,
    SESSION_LAUNCHES,
    MOVE_BACKUPS,
    ROOT_MIGRATIONS,
];

/// Every legacy (version 1) top-level entry and its version 2 location. The
/// registry database's `-wal`/`-shm` companions move with it.
pub const MOVES: &[(&str, &str)] = &[
    ("registry.db", REGISTRY_DB),
    ("session-access.json", SESSION_ACCESS),
    ("session-access.lock", SESSION_ACCESS_LOCK),
    ("account-catalog", ACCOUNT_CATALOG),
    ("session-catalog", SESSION_CATALOG),
    ("conversations", CONVERSATIONS),
    ("thread-locations", THREAD_LOCATIONS),
    ("managed-panes", MANAGED_PANES),
    ("herdr-panes", HERDR_PANES),
    ("provider-state", PROVIDER_STATE),
    ("archived-sessions", ARCHIVED_SESSIONS),
    ("preserved-conversations", PRESERVED_CONVERSATIONS),
    ("claude-hosted", CLAUDE_HOSTED),
    ("launches", LAUNCHES),
    ("resumptions", RESUMPTIONS),
    ("deletions", DELETIONS),
    ("account-deletions", ACCOUNT_DELETIONS),
    ("conversation-operations", CONVERSATION_OPERATIONS),
    ("handoff-jobs", HANDOFF_JOBS),
    ("space-moves", SPACE_MOVES),
    ("lifecycle-admission", LIFECYCLE_ADMISSION),
    ("message-recoveries", MESSAGE_RECOVERIES),
    ("move-backups", MOVE_BACKUPS),
    ("project-relocation.lock", PROJECT_RELOCATION_LOCK),
    ("claude-steer", CLAUDE_STEER),
    ("claude-deliveries", CLAUDE_DELIVERIES),
    ("questions", QUESTIONS),
    ("observations", OBSERVATIONS),
    (
        "state-snapshot-active.json",
        "cache/state-snapshot-active.json",
    ),
    ("state-snapshot-all.json", "cache/state-snapshot-all.json"),
    (
        "state-snapshot-archived.json",
        "cache/state-snapshot-archived.json",
    ),
    ("compatibility-report.json", COMPATIBILITY_REPORT),
    ("daemon.log", DAEMON_LOG),
    ("handoff.log", HANDOFF_LOG),
];

/// Legacy entries whose absolute paths are recorded inside durable journals
/// (archive manifests, account-deletion records, move backups). A relative
/// compatibility symlink stays at the legacy path so recorded paths resolve.
pub const COMPAT_LINKS: &[&str] = &[
    "archived-sessions",
    "preserved-conversations",
    "move-backups",
];

/// Version 1 parents of non-Codex managed homes.
pub const LEGACY_HOME_PARENTS: &[(ProviderId, &str)] = &[
    (ProviderId::Claude, "claude-accounts"),
    (ProviderId::Gemini, "gemini-accounts"),
    (ProviderId::Antigravity, "antigravity-accounts"),
];

/// `accounts/<platform>` directory names; never Codex account names in version 1.
pub fn is_platform_dir(name: &str) -> bool {
    ProviderId::ALL.iter().any(|p| p.as_str() == name)
}

/// The version 2 location of an account home.
pub fn target_home(root: &Path, provider: ProviderId, name: &str) -> PathBuf {
    root.join(ACCOUNTS).join(provider.as_str()).join(name)
}
/// The version 1 location of an account home (`None` where it cannot exist).
pub fn legacy_home(root: &Path, provider: ProviderId, name: &str) -> Option<PathBuf> {
    let bare = crate::provider::ops::facts(provider).bare_name;
    match provider {
        _ if bare && is_platform_dir(name) => None,
        _ if bare => Some(root.join(ACCOUNTS).join(name)),
        p => LEGACY_HOME_PARENTS
            .iter()
            .find(|(q, _)| *q == p)
            .map(|(_, parent)| root.join(parent).join(name)),
    }
}
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

/// The managed home of an account: `accounts/<platform>/<name>`, or its legacy
/// path while that home's move is deferred (the legacy path is then a real
/// directory and the target does not exist).
pub fn account_home(root: &Path, provider: ProviderId, name: &str) -> PathBuf {
    let target = target_home(root, provider, name);
    if fs::symlink_metadata(&target).is_err()
        && let Some(legacy) = legacy_home(root, provider, name)
        && real_dir(&legacy)
    {
        return legacy;
    }
    target
}

/// Create (0700) the parents of a new managed home for `provider`.
pub fn ensure_home_parent(root: &Path, provider: ProviderId) -> Result<()> {
    private_dir(&root.join(ACCOUNTS))?;
    private_dir(&root.join(ACCOUNTS).join(provider.as_str()))
}

/// Codex account homes: `accounts/codex/<name>` plus deferred legacy homes
/// (`accounts/<name>`, real directories only). Sorted by name, unique.
pub fn codex_homes(root: &Path) -> Vec<(String, PathBuf)> {
    let mut homes = std::collections::BTreeMap::new();
    let codex = root.join(ACCOUNTS).join(ProviderId::Codex.as_str());
    for (dir, legacy) in [(root.join(ACCOUNTS), true), (codex, false)] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if crate::paths::validate_name(&name).is_err()
                || (legacy && is_platform_dir(&name))
                || !entry.file_type().is_ok_and(|t| t.is_dir())
            {
                continue;
            }
            homes.insert(name, entry.path());
        }
    }
    homes.into_iter().collect()
}

/// Whether two home paths name the same directory: equal as given, or equal
/// after resolving symlinks (a legacy compatibility path and its target) and
/// mapping both through the root alias ([`crate::alias::logical`]).
pub fn same_home(a: &Path, b: &Path) -> bool {
    a == b || resolve_path(a) == resolve_path(b) // raw-path: both sides resolved
}

/// Resolve symlinks of the longest existing ancestor and re-append the rest,
/// so a path under a legacy compatibility symlink equals its canonical form
/// even when the final components do not exist yet. The result is in logical
/// form: with an active root alias both spellings resolve to the same path.
pub fn resolve_path(path: &Path) -> PathBuf {
    crate::alias::logical(&resolve_physical(path))
}
fn resolve_physical(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        // raw-path: resolve_path applies logical() to the result
        if let Ok(real) = fs::canonicalize(cur) {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Rewrite stored absolute home paths after homes moved (`from` to `to`).
/// Only exact home matches change: `conversations/*.json` `owner`,
/// `managed-panes/*.json` `home` and `herdr-panes/*.json` `binding.home`.
/// Journals and receipts are never rewritten. Idempotent; returns the number
/// of records changed.
pub fn rewrite_home_refs(root: &Path, moves: &[(PathBuf, PathBuf)]) -> Result<usize> {
    let mut changed = 0;
    for (dir, field) in [
        (CONVERSATIONS, "/owner"),
        (MANAGED_PANES, "/home"),
        (HERDR_PANES, "/binding/home"),
    ] {
        let Ok(entries) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json")
                || !entry.file_type().is_ok_and(|t| t.is_file())
            {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let target = value
                .pointer(field)
                .and_then(|v| v.as_str())
                .and_then(|s| moves.iter().find(|(from, _)| from.as_os_str() == s));
            if let Some((_, to)) = target {
                *value.pointer_mut(field).expect("checked") = serde_json::json!(to);
                crate::paths::atomic_write(&path, &serde_json::to_vec(&value)?)?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Complete,
    Migrating,
    RollingBack,
}

/// A home left at its legacy path because it was in use.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Deferred {
    pub provider: String,
    pub name: String,
    pub legacy: String,
    pub target: String,
    pub reason: String,
    pub since: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    #[serde(default)]
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// Relative paths moved from their legacy location (stores and homes).
    #[serde(default)]
    pub moved: Vec<(String, String)>,
    /// Legacy paths left as relative symlinks to their new location.
    #[serde(default)]
    pub compat_links: Vec<String>,
    #[serde(default)]
    pub deferred: Vec<Deferred>,
    #[serde(default)]
    pub history: Vec<serde_json::Value>,
    /// Root alias (contract 1.54.0); required by version 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_alias: Option<RootAlias>,
}

/// The `root_alias` block of `layout.json` (written by `root migrate`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootAlias {
    pub version: u32,
    /// pending | active | inactive
    pub state: String,
    /// Written once; `Paths::resolve` accepts a redirect when both spellings show it.
    pub root_id: String,
    pub canonical_root: PathBuf,
    pub previous_root: PathBuf,
    pub entries: Vec<crate::alias::Entry>,
    #[serde(default)]
    pub normalize_requests: bool,
    #[serde(default)]
    pub run: String,
    #[serde(default)]
    pub backup: PathBuf,
    #[serde(default)]
    pub activated_at: Option<i64>,
}
impl RootAlias {
    pub fn is_active(&self) -> bool {
        self.state == "active"
    }
}

pub fn read_record(root: &Path) -> Result<Option<Record>> {
    match fs::read(root.join(LAYOUT_FILE)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("LAYOUT_INVALID: layout.json is unreadable")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn write_record(root: &Path, record: &Record) -> Result<()> {
    crate::paths::atomic_write(&root.join(LAYOUT_FILE), &serde_json::to_vec_pretty(record)?)
}

/// Legacy (version 1) content: any legacy top-level store or account parent,
/// or a Codex home directly under `accounts/`.
pub fn legacy_entries(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = MOVES
        .iter()
        .map(|(legacy, _)| *legacy)
        .chain(LEGACY_HOME_PARENTS.iter().map(|(_, p)| *p))
        .filter(|name| fs::symlink_metadata(root.join(name)).is_ok())
        .map(str::to_owned)
        .collect();
    if let Ok(entries) = fs::read_dir(root.join(ACCOUNTS)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_platform_dir(&name) && entry.file_type().is_ok_and(|t| t.is_dir()) {
                found.push(format!("{ACCOUNTS}/{name}"));
            }
        }
    }
    found
}

/// Create the version 2 top-level directories (0700).
pub fn ensure_dirs(root: &Path) -> Result<()> {
    for dir in [
        REGISTRY, SESSIONS, OPERATIONS, MESSAGING, CACHE, LOGS, ACCOUNTS,
    ] {
        private_dir(&root.join(dir))?;
    }
    Ok(())
}
/// Parents of files opened in place (locks, logs) on a new root; `messaging/`
/// and `accounts/` are created by their first writer.
fn ensure_store_dirs(root: &Path) -> Result<()> {
    for dir in [REGISTRY, SESSIONS, OPERATIONS, CACHE, LOGS] {
        private_dir(&root.join(dir))?;
    }
    Ok(())
}

/// Admit this build to a data root. A root with a complete version 2 record is
/// used as is. A root without a record is stamped as version 2 when it holds
/// no legacy content (a new root); a legacy root fails closed until migrated.
pub fn admit(root: &Path) -> Result<()> {
    match read_record(root)? {
        Some(r) if r.version > MAX_VERSION => bail!(
            "LAYOUT_UNSUPPORTED: data root layout version {} is newer than this build supports ({MAX_VERSION})",
            r.version
        ),
        Some(r)
            if r.version == ALIAS_VERSION
                && r.state == State::Complete
                && !r.root_alias.as_ref().is_some_and(RootAlias::is_active) =>
        {
            bail!(
                "LAYOUT_INVALID: layout version {ALIAS_VERSION} requires an active root_alias block; run `sessionsation root status`"
            )
        }
        Some(r)
            if (r.version == VERSION || r.version == ALIAS_VERSION)
                && r.state == State::Complete =>
        {
            if !root.join(LOGS).is_dir() {
                ensure_store_dirs(root)?;
            }
            Ok(())
        }
        Some(r) if r.state != State::Complete && r.root_alias.is_some() => bail!(
            "LAYOUT_MIGRATION_IN_PROGRESS: a `root {}` did not finish; rerun it (or `sessionsation root rollback`) with the supervisor stopped",
            if r.state == State::Migrating {
                "migrate"
            } else {
                "rollback"
            }
        ),
        Some(r) if r.state != State::Complete => bail!(
            "LAYOUT_MIGRATION_IN_PROGRESS: a data-root layout {} did not finish; run `sessionsation layout migrate` (or `layout rollback`) with the supervisor stopped",
            if r.state == State::Migrating {
                "migration"
            } else {
                "rollback"
            }
        ),
        Some(_) => bail!(
            "LAYOUT_MIGRATION_REQUIRED: run `sessionsation layout migrate` with the supervisor stopped"
        ),
        None => {
            let legacy = legacy_entries(root);
            if !legacy.is_empty() {
                bail!(
                    "LAYOUT_MIGRATION_REQUIRED: {} uses the legacy (version 1) layout ({}); stop the supervisor and run `sessionsation layout migrate`",
                    root.display(),
                    legacy
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            ensure_store_dirs(root)?;
            write_record(
                root,
                &Record {
                    version: VERSION,
                    created_at: Some(now()),
                    ..Record::default()
                },
            )
        }
    }
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_root_is_stamped_and_legacy_root_fails_closed() {
        let t = tempfile::tempdir().unwrap();
        admit(t.path()).unwrap();
        let r = read_record(t.path()).unwrap().unwrap();
        assert_eq!((r.version, r.state), (VERSION, State::Complete));
        let legacy = tempfile::tempdir().unwrap();
        fs::write(legacy.path().join("registry.db"), "").unwrap();
        let e = admit(legacy.path()).unwrap_err().to_string();
        assert!(e.starts_with("LAYOUT_MIGRATION_REQUIRED"), "{e}");
        assert!(!legacy.path().join(LAYOUT_FILE).exists());
    }

    #[test]
    fn deferred_legacy_homes_resolve_until_moved() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        assert_eq!(
            account_home(root, ProviderId::Gemini, "g"),
            root.join("accounts/gemini/g")
        );
        fs::create_dir_all(root.join("gemini-accounts/g")).unwrap();
        fs::create_dir_all(root.join("accounts/work")).unwrap();
        assert_eq!(
            account_home(root, ProviderId::Gemini, "g"),
            root.join("gemini-accounts/g")
        );
        assert_eq!(
            account_home(root, ProviderId::Codex, "work"),
            root.join("accounts/work")
        );
        // A platform directory is never a legacy Codex home.
        fs::create_dir_all(root.join("accounts/claude/x")).unwrap();
        assert_eq!(
            account_home(root, ProviderId::Codex, "claude"),
            root.join("accounts/codex/claude")
        );
        fs::create_dir_all(root.join("accounts/codex/new")).unwrap();
        let names: Vec<_> = codex_homes(root).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["new", "work"]);
        // Once moved, the target wins even though a compatibility link remains.
        fs::rename(
            root.join("gemini-accounts/g"),
            root.join("accounts/gemini-g"),
        )
        .unwrap();
        fs::create_dir_all(root.join("accounts/gemini")).unwrap();
        fs::rename(
            root.join("accounts/gemini-g"),
            root.join("accounts/gemini/g"),
        )
        .unwrap();
        std::os::unix::fs::symlink("../accounts/gemini/g", root.join("gemini-accounts/g")).unwrap();
        assert_eq!(
            account_home(root, ProviderId::Gemini, "g"),
            root.join("accounts/gemini/g")
        );
        assert!(same_home(
            &root.join("gemini-accounts/g"),
            &root.join("accounts/gemini/g")
        ));
    }
}
