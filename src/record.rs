//! One engine for journaled operation records (guard role `records`): `<root>/<DIR>/<id>.json`,
//! pretty JSON written by [`crate::paths::atomic_write`], an optional directory-wide lock for
//! short read-modify-writes, and the per-record `<id>.worker.lock` a detached worker holds while
//! it runs. File names, directories and serde shapes are each kind's own (old binaries read new
//! records and the other way round). Parsed records are cached by file version
//! ([`crate::file_cache`]), so a settled record is parsed once.
use crate::{file_cache::FileCache, paths::Paths};
use anyhow::{Context, Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{any::Any, fs, path::PathBuf};

type Parsed = Box<dyn Any + Send + Sync>;
static CACHE: FileCache<Parsed> = FileCache::new();

/// A journaled operation kind. Only the directory, the id and the list order are required.
pub trait Journal: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {
    /// Directory under the data root (a `layout` constant).
    const DIR: &'static str;
    /// Directory-wide lock of [`Journal::update`] (a `layout` constant); `None`: unlocked.
    const LOCK: Option<&'static str> = None;
    /// Error context of a missing record.
    const NOT_FOUND: &'static str = "OPERATION_NOT_FOUND";
    /// [`Journal::save`] stamps `updated_at`; when false only [`Journal::update`] does.
    const STAMP_ON_SAVE: bool = true;
    /// [`Journal::list`] reads the directory plainly (unreadable entries skipped) instead of
    /// through [`crate::paths::journal_paths`] (worker-lock and log entries, errors raised).
    const PLAIN_LIST: bool = false;
    /// Error context of an unparsable record (`None`: the parser's own message).
    const UNREADABLE: Option<&'static str> = Some("UNREADABLE_OPERATION_RECORD");
    fn id(&self) -> &str;
    /// List order: `(created_at, id)` ascending.
    fn key(&self) -> (i64, &str);
    /// Stamp `updated_at` before every save.
    fn touch(&mut self, now: i64);
    /// A record id is a UUID (the file stem).
    fn check_id(id: &str) -> Result<()> {
        uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
        Ok(())
    }

    fn dir(paths: &Paths) -> PathBuf {
        paths.root.join(Self::DIR)
    }
    fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
        Self::check_id(id)?;
        Ok(Self::dir(paths).join(format!("{id}.json")))
    }
    /// The lock a detached worker holds while it runs (`<id>.worker.lock`).
    fn worker_lock(paths: &Paths, id: &str) -> Result<PathBuf> {
        Ok(Self::file(paths, id)?.with_extension("worker.lock"))
    }
    /// No worker holds the record's worker lock (a missing lock counts as stopped).
    fn worker_stopped(paths: &Paths, id: &str) -> Result<bool> {
        crate::background::worker_stopped(&Self::worker_lock(paths, id)?)
    }
    fn exists(paths: &Paths, id: &str) -> bool {
        Self::file(paths, id).is_ok_and(|f| f.is_file())
    }
    fn read(paths: &Paths, id: &str) -> Result<Self> {
        use crate::file_cache::LoadError;
        let parsed = CACHE.load(&Self::file(paths, id)?, |b| {
            serde_json::from_slice::<Self>(b).map(|r| Box::new(r) as Parsed)
        });
        match parsed {
            Ok(r) => r
                .downcast_ref::<Self>()
                .cloned()
                .context("UNREADABLE_OPERATION_RECORD"),
            Err(LoadError::Io(e)) => Err(anyhow::Error::new(e).context(Self::NOT_FOUND)),
            Err(LoadError::Parse(e)) => Err(match Self::UNREADABLE {
                Some(code) => anyhow::Error::new(e).context(code),
                None => e.into(),
            }),
        }
    }
    /// Stamp (unless [`Journal::STAMP_ON_SAVE`] is false) and replace the record.
    fn save(&mut self, paths: &Paths) -> Result<()> {
        if Self::STAMP_ON_SAVE {
            self.touch(crate::daemon::now());
        }
        self.write(paths)
    }
    /// Replace the record atomically, as it is.
    fn write(&self, paths: &Paths) -> Result<()> {
        crate::paths::atomic_write(
            &Self::file(paths, self.id())?,
            &serde_json::to_vec_pretty(self)?,
        )
    }
    /// Every readable record in list order (unreadable ones are skipped).
    fn list(paths: &Paths) -> Result<Vec<Self>> {
        let dir = Self::dir(paths);
        if !dir.is_dir() {
            return Ok(vec![]);
        }
        let files = if Self::PLAIN_LIST {
            plain_files(fs::read_dir(&dir).map(|d| d.map(|e| e.map(|e| e.path()))))
        } else {
            crate::paths::journal_paths(&dir)?
        };
        CACHE.retain_dir(&dir, &files.iter().map(|e| e.as_path()).collect());
        let mut all: Vec<Self> = files
            .iter()
            .filter_map(|f| Self::read(paths, f.file_stem()?.to_str()?).ok())
            .collect();
        all.sort_by(|a, b| a.key().cmp(&b.key()));
        Ok(all)
    }
    /// The directory-wide lock, held for one short read-modify-write (never across an await).
    fn lock(paths: &Paths) -> Result<Option<fs::File>> {
        let Some(name) = Self::LOCK else {
            return Ok(None);
        };
        lock_exclusive(&paths.root.join(name)).map(Some)
    }
    /// Apply `edit` to the stored record when it returns true; `None` when it declined. Every
    /// transition goes through this, so a concurrent cancel or expiry is never overwritten.
    fn update(
        paths: &Paths,
        id: &str,
        edit: impl FnOnce(&mut Self) -> bool,
    ) -> Result<Option<Self>> {
        let _lock = Self::lock(paths)?;
        let mut r = Self::read(paths, id)?;
        if !edit(&mut r) {
            return Ok(None);
        }
        r.touch(crate::daemon::now());
        r.write(paths)?;
        Ok(Some(r))
    }
}

/// The `.json` files of a plain directory listing: an unreadable directory lists nothing and an
/// unreadable entry is skipped (never an error).
fn plain_files(
    entries: std::io::Result<impl Iterator<Item = std::io::Result<PathBuf>>>,
) -> Vec<PathBuf> {
    let json = |p: &PathBuf| p.extension().is_some_and(|x| x == "json");
    entries.map_or_else(|_| vec![], |e| e.flatten().filter(json).collect())
}
/// A lock file in a private parent directory, locked exclusively (blocking).
pub fn lock_exclusive(path: &std::path::Path) -> Result<fs::File> {
    crate::paths::private_dir(path.parent().context("lock parent")?)?;
    let f = crate::paths::lock_file(path)?;
    fs2::FileExt::lock_exclusive(&f)?;
    Ok(f)
}
/// Read a JSON file written by [`store`] (`None` when missing or unreadable).
pub fn load<T: DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}
/// Replace a compact JSON file atomically (records outside `operations/`).
pub fn store<T: Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    crate::paths::atomic_write(path, &serde_json::to_vec(value)?)
}

// ------------------------------------------------------------ idle-point operations

/// Phases that end an idle-point operation.
pub const TERMINAL: &[&str] = &["complete", "failed", "cancelled", "expired"];
/// A release claim older than this is taken over (its process died).
const RELEASE_CLAIM_SECONDS: i64 = 120;

/// The journal of an idle-point operation (a compaction or a model change, see [`idle_op!`]):
/// `<dir>/<id>.json` under a directory-wide lock, `updated_at` stamped by [`Journal::update`].
macro_rules! idle_op {
    ($record:ty, $dir:ident, $lock:ident) => {
        impl crate::record::Journal for $record {
            const DIR: &'static str = crate::layout::$dir;
            const LOCK: Option<&'static str> = Some(crate::layout::$lock);
            const STAMP_ON_SAVE: bool = false;
            fn id(&self) -> &str {
                &self.id
            }
            fn key(&self) -> (i64, &str) {
                (self.created_at, &self.id)
            }
            fn touch(&mut self, now: i64) {
                self.updated_at = now;
            }
        }
        impl crate::record::IdleOp for $record {
            fn fields(&mut self) -> crate::record::Fields<'_> {
                crate::record::Fields {
                    phase: &mut self.phase,
                    reason: &mut self.reason,
                    finished_at: &mut self.finished_at,
                    hold_released: &mut self.hold_released,
                    release_claimed_at: &mut self.release_claimed_at,
                }
            }
            fn phase(&self) -> &str {
                &self.phase
            }
            fn conversation(&self) -> &str {
                &self.conversation_id
            }
            fn updated_at(&self) -> i64 {
                self.updated_at
            }
        }
    };
}
pub(crate) use idle_op;

/// The mutable lifecycle fields every idle-point operation has.
pub(crate) struct Fields<'a> {
    pub phase: &'a mut String,
    pub reason: &'a mut Option<String>,
    pub finished_at: &'a mut Option<i64>,
    pub hold_released: &'a mut bool,
    pub release_claimed_at: &'a mut Option<i64>,
}
/// Lifecycle shared by compactions and model changes (`waiting` until an idle point, then a
/// provider-specific run, then one of [`TERMINAL`]; held input released once afterwards).
pub(crate) trait IdleOp: Journal {
    fn fields(&mut self) -> Fields<'_>;
    fn phase(&self) -> &str;
    fn conversation(&self) -> &str;
    fn updated_at(&self) -> i64;
    fn done(&self) -> bool {
        TERMINAL.contains(&self.phase())
    }
    /// End the operation when it is still in one of `from`.
    fn finish(paths: &Paths, id: &str, from: &[&str], phase: &str, reason: &str) {
        let _ = Self::update(paths, id, |r| {
            let f = r.fields();
            if !from.contains(&f.phase.as_str()) {
                return false;
            }
            *f.phase = phase.into();
            *f.reason = Some(reason.into());
            *f.finished_at = Some(crate::daemon::now());
            true
        });
    }
    /// Record why a waiting operation has not started yet.
    fn waiting(paths: &Paths, id: &str, why: &str) {
        let _ = Self::update(paths, id, |r| {
            let f = r.fields();
            if f.phase != "waiting" || f.reason.as_deref() == Some(why) {
                return false;
            }
            *f.reason = Some(why.into());
            true
        });
    }
    /// A non-terminal operation of the conversation requested before `key` (all with `None`).
    fn pending_before(paths: &Paths, conversation: &str, key: Option<(i64, &str)>) -> bool {
        Self::list(paths).is_ok_and(|all| {
            all.iter().any(|r| {
                !r.done() && r.conversation() == conversation && key.is_none_or(|k| r.key() < k)
            })
        })
    }
    /// `--operation ID`, else the session's newest pending operation, else its newest one.
    fn find(paths: &Paths, operation: Option<&str>, session: Option<&str>) -> Result<Self> {
        let session = match (operation, session) {
            (Some(id), _) => return Self::read(paths, id),
            (None, Some(session)) => crate::catalog::find(paths, session)?,
            _ => bail!("INVALID_ARGUMENT: --operation or --session is required"),
        };
        let mut mine: Vec<Self> = Self::list(paths)?
            .into_iter()
            .filter(|r| r.conversation() == session.conversation_id)
            .collect();
        let pick = mine
            .iter()
            .rposition(|r| !r.done())
            .or(mine.len().checked_sub(1));
        Ok(mine.swap_remove(pick.context("OPERATION_NOT_FOUND")?))
    }
    /// `api operations` rows: pending ones and those finished within the last hour.
    fn rows(paths: &Paths) -> Result<Vec<Self>> {
        let now = crate::daemon::now();
        Ok(Self::list(paths)?
            .into_iter()
            .filter(|r| !r.done() || now - r.updated_at() < 3600)
            .collect())
    }
    /// Cancel a waiting operation: `(record, true)` when this call cancelled it. Idempotent for
    /// a cancelled one; `active` once it has started, `finished` after it ended otherwise.
    async fn cancel(paths: &Paths, id: &str, active: &str, finished: &str) -> Result<(Self, bool)> {
        let r = Self::read(paths, id)?;
        match r.phase() {
            "cancelled" => return Ok((r, false)),
            "waiting" => {}
            p if TERMINAL.contains(&p) => bail!("{finished}"),
            _ => bail!("{active}"),
        }
        let cancelled = Self::update(paths, id, |r| {
            let f = r.fields();
            if f.phase != "waiting" {
                return false;
            }
            *f.phase = "cancelled".into();
            *f.reason = Some("cancelled".into());
            *f.finished_at = Some(crate::daemon::now());
            true
        })?;
        if cancelled.is_none() {
            // Lost a race with the start or the expiry: report what actually happened.
            let now = Self::read(paths, id)?;
            ensure!(now.phase() == "cancelled", "{active}");
            return Ok((now, false));
        }
        release::<Self>(paths, id).await;
        Ok((Self::read(paths, id)?, true))
    }
}

/// Release the input held for a finished operation. `hold_released` is set only after a pass
/// found nothing left to deliver or retry; until then the hold stays in force, so a new send
/// cannot overtake held ones, and the next tick tries again. A concurrent release (another
/// process) is skipped by a claim that lapses after `RELEASE_CLAIM_SECONDS`.
pub(crate) async fn release<R: IdleOp>(paths: &Paths, id: &str) {
    let claimed = R::update(paths, id, |r| {
        let now = crate::daemon::now();
        let done = r.done();
        let f = r.fields();
        if !done
            || *f.hold_released
            || f.release_claimed_at
                .is_some_and(|at| now - at < RELEASE_CLAIM_SECONDS)
        {
            return false;
        }
        *f.release_claimed_at = Some(now);
        true
    });
    let Ok(Some(r)) = claimed else { return };
    let done = crate::messaging::compaction::deliver_held(paths, r.conversation()).await;
    let _ = R::update(paths, id, |r| {
        let f = r.fields();
        *f.hold_released |= done;
        *f.release_claimed_at = None;
        true
    });
}

#[cfg(test)]
mod tests;
