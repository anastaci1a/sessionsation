use crate::{
    config::Config,
    model::State,
    paths::{Paths, atomic_write, lock_file},
};
use anyhow::{Context, Result};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, params};
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
        let db = Connection::open(paths.root.join("registry.db"))?;
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
    pub fn load(&self) -> Result<State> {
        let json: Result<String, _> =
            self.db
                .query_row("SELECT json FROM state WHERE id=1", [], |r| r.get(0));
        match json {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
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
pub fn load_state_snapshot(paths: &Paths) -> Result<State> {
    let db = Connection::open_with_flags(
        paths.root.join("registry.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let json: String = db.query_row("SELECT json FROM state WHERE id=1", [], |r| r.get(0))?;
    Ok(serde_json::from_str(&json)?)
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
