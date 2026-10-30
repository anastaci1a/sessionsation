use crate::{
    config::Config,
    model::State,
    paths::{Paths, atomic_write, lock_file},
};
use anyhow::{Context, Result};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use std::fs::File;

pub struct Store {
    pub(crate) db: Connection,
    _lock: File,
}
impl Store {
    /// Only the daemon writes the registry. The lock also protects stale-socket cleanup.
    pub fn open(paths: &Paths) -> Result<Self> {
        let lock = lock_file(&paths.root.join("daemon.lock"))?;
        lock.try_lock_exclusive()
            .context("another supervisor owns this data directory")?;
        crate::paths::private_dir(&paths.root.join(crate::layout::REGISTRY))?;
        let db = Connection::open(paths.root.join(crate::layout::REGISTRY_DB))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL);
            INSERT INTO schema_version SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM schema_version);
            CREATE TABLE IF NOT EXISTS state(id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL, entity TEXT NOT NULL);")?;
        let version: i64 = db.query_row("SELECT version FROM schema_version", [], |r| r.get(0))?;
        anyhow::ensure!(version == 1, "unsupported registry version {version}");
        crate::usage_history::initialize(&db)?;
        Ok(Self { db, _lock: lock })
    }
    /// Load the registry, applying and persisting (once, with one event per
    /// account) the legacy provider migration.
    pub fn load(&self) -> Result<State> {
        let json: Result<String, _> =
            self.db
                .query_row("SELECT json FROM state WHERE id=1", [], |r| r.get(0));
        let mut value: Value = match json {
            Ok(s) => serde_json::from_str(&s)?,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(State::default()),
            Err(e) => return Err(e.into()),
        };
        let migrated = migrate_legacy_codex_provider(&mut value);
        let rekeyed = migrate_account_keys(&mut value);
        let state: State = serde_json::from_value(value)?;
        if !migrated.is_empty() || !rekeyed.is_empty() {
            let tx = self.db.unchecked_transaction()?;
            tx.execute(
                "UPDATE state SET json=?1 WHERE id=1",
                [serde_json::to_string(&state)?],
            )?;
            let now = crate::daemon::now();
            for name in &migrated {
                tx.execute(
                    "INSERT INTO events(at,kind,entity) VALUES(?1,?2,?3)",
                    params![now, LEGACY_CODEX_PROVIDER_EVENT, name],
                )?;
            }
            for name in &rekeyed {
                tx.execute(
                    "INSERT INTO events(at,kind,entity) VALUES(?1,?2,?3)",
                    params![now, ACCOUNT_KEY_EVENT, name],
                )?;
            }
            tx.commit()?;
        }
        Ok(state)
    }
    pub fn save(&mut self, state: &State, now: i64, kind: &str, entity: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO state VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
            [serde_json::to_string(state)?],
        )?;
        tx.execute(
            "INSERT INTO events(at,kind,entity) VALUES(?1,?2,?3)",
            params![now, kind, entity],
        )?;
        tx.commit()?;
        Ok(())
    }
}
/// Read-only snapshot; a legacy registry is migrated in memory only (the
/// supervisor, as the single writer, persists the migration).
pub fn load_state_snapshot(paths: &Paths) -> Result<State> {
    let db = Connection::open_with_flags(
        paths.root.join(crate::layout::REGISTRY_DB),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let json: String = db.query_row("SELECT json FROM state WHERE id=1", [], |r| r.get(0))?;
    let mut value: Value = serde_json::from_str(&json)?;
    migrate_legacy_codex_provider(&mut value);
    migrate_account_keys(&mut value);
    Ok(serde_json::from_value(value)?)
}

pub const LEGACY_CODEX_PROVIDER_EVENT: &str = "account.provider_migrated_legacy_codex";
/// Registries written before providers existed could enroll only Codex
/// (`account add` always started a Codex device login), so an account record
/// without `provider` is a Codex account. This is the only place that
/// assigns a provider to a legacy record; names are never used to infer it.
/// Returns the migrated account names; idempotent.
pub fn migrate_legacy_codex_provider(state: &mut Value) -> Vec<String> {
    let mut migrated = vec![];
    if let Some(accounts) = state.get_mut("accounts").and_then(Value::as_object_mut) {
        for (name, account) in accounts.iter_mut() {
            if let Some(record) = account.as_object_mut()
                && !record.contains_key("provider")
            {
                record.insert("provider".into(), Value::String("codex".into()));
                migrated.push(name.clone());
            }
        }
    }
    migrated
}
pub fn load_config(paths: &Paths) -> Result<Config> {
    let path = paths.root.join("config.json");
    let mut config: Config = if path.exists() {
        serde_json::from_slice(&std::fs::read(path)?)?
    } else {
        Config::default()
    };
    // Older configurations could expire a reading before the next routine
    // poll. Migrate that interval in memory so existing installs still load.
    if config.stale_seconds < config.idle_poll_seconds.saturating_add(90) {
        config.stale_seconds = config.idle_poll_seconds.saturating_add(90);
    }
    config.validate()?;
    Ok(config)
}
pub fn save_config(paths: &Paths, config: &Config) -> Result<()> {
    config.validate()?;
    atomic_write(
        &paths.root.join("config.json"),
        &serde_json::to_vec_pretty(config)?,
    )
}

pub const ACCOUNT_KEY_EVENT: &str = "account.key_migrated_per_provider";
/// Account names are unique per provider (contract 1.30.0): a non-Codex record
/// is keyed `<provider>:<name>` so it can coexist with a Codex account of the
/// same name. Registries written before that keyed every record by bare name.
/// Returns the migrated record names; idempotent.
pub fn migrate_account_keys(state: &mut Value) -> Vec<String> {
    let mut migrated = vec![];
    if let Some(accounts) = state.get_mut("accounts").and_then(Value::as_object_mut) {
        let moves: Vec<(String, String)> = accounts
            .iter()
            .filter_map(|(key, record)| {
                let provider = record.get("provider")?.as_str()?;
                let name = record.get("name")?.as_str()?;
                let want = if provider == "codex" {
                    name.to_owned()
                } else {
                    format!("{provider}:{name}")
                };
                (want != *key).then(|| (key.clone(), want))
            })
            .collect();
        for (old, new) in moves {
            if accounts.contains_key(&new) {
                continue;
            }
            if let Some(record) = accounts.remove(&old) {
                accounts.insert(new.clone(), record);
                migrated.push(new);
            }
        }
    }
    migrated
}
