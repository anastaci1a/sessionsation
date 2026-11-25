//! Native last-turn evidence indexed by unchanged local history, never by a PID
//! or elapsed time. Fresh native status checks remain mandatory on every use.
use super::*;
use std::{os::unix::fs::MetadataExt, path::Path, time::Duration};

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let m = std::fs::metadata(path).ok()?;
    (m.is_file() && m.uid() == unsafe { libc::geteuid() }).then(|| Fingerprint {
        dev: m.dev(),
        ino: m.ino(),
        size: m.len(),
        modified: (m.mtime(), m.mtime_nsec()),
        changed: (m.ctime(), m.ctime_nsec()),
    })
}
#[derive(Serialize, Deserialize)]
struct Saved {
    generation: String,
    history: PathBuf,
    fingerprint: Fingerprint,
    observed_at: i64,
    page: Page,
}
pub(super) struct Cache {
    file: PathBuf,
    history: PathBuf,
    fingerprint: Fingerprint,
    generation: String,
}
impl Cache {
    pub(super) fn new(paths: &Paths, a: &Attachment, history: Option<&Path>) -> Option<Self> {
        let history = crate::alias::canonical(history?).ok()?;
        let home = crate::alias::canonical(&a.home).ok()?;
        // raw-path: both sides are alias::canonical forms (above)
        if !history.starts_with(home.join("sessions"))
            // raw-path: both sides are alias::canonical forms (above)
            && !history.starts_with(home.join("archived_sessions"))
        {
            return None;
        }
        let fingerprint = fingerprint(&history)?;
        Some(Self {
            file: paths
                .root
                .join(crate::layout::OBSERVATIONS)
                .join("native-turns")
                .join(format!("{}.json", a.id)),
            history,
            fingerprint,
            generation: a.generation(),
        })
    }
    pub(super) fn unchanged(&self) -> bool {
        fingerprint(&self.history).as_ref() == Some(&self.fingerprint)
    }
    fn load(&self) -> Option<Saved> {
        let saved: Saved = serde_json::from_slice(&std::fs::read(&self.file).ok()?).ok()?;
        (saved.generation == self.generation
            && saved.history == self.history
            && saved.fingerprint == self.fingerprint
            && self.unchanged()
            && saved.observed_at <= crate::daemon::now()
            && crate::daemon::now() < saved.observed_at + 5
            && terminal(&saved.page))
        .then_some(saved)
    }
    pub(super) async fn get(&self, control: &mut Transport, id: &str) -> Result<(Page, Value)> {
        crate::paths::private_dir(self.file.parent().ok_or(Fault("storage_error"))?)
            .map_err(|_| Fault("storage_error"))?;
        let lock = crate::paths::lock_file(&self.file.with_extension("lock"))
            .map_err(|_| Fault("storage_error"))?;
        // Coalesce concurrent readers without blocking the async executor. The
        // public caller's unchanged five-second deadline bounds this wait too.
        loop {
            if let Some(saved) = self.load() {
                return Ok((
                    saved.page,
                    json!({"source":"unchanged_history_cache","observed_at":saved.observed_at}),
                ));
            }
            if !self.unchanged() {
                return Err(Fault("history_changed_during_observation"));
            }
            if lock.try_lock_exclusive().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if let Some(saved) = self.load() {
            return Ok((
                saved.page,
                json!({"source":"unchanged_history_cache","observed_at":saved.observed_at}),
            ));
        }
        let page: Page = control.passive_summary(id).await?;
        let observed_at = crate::daemon::now();
        if !self.unchanged() {
            return Err(Fault("history_changed_during_observation"));
        }
        if terminal(&page) {
            // This stores historical evidence only, not readiness. A caller
            // must still recheck native status, history and runtime identity.
            let saved = Saved {
                generation: self.generation.clone(),
                history: self.history.clone(),
                fingerprint: self.fingerprint.clone(),
                observed_at,
                page: page.clone(),
            };
            crate::paths::atomic_write(
                &self.file,
                &serde_json::to_vec(&saved).map_err(|_| Fault("storage_error"))?,
            )
            .map_err(|_| Fault("storage_error"))?;
        }
        Ok((
            page,
            json!({"source":"native_summary","observed_at":observed_at}),
        ))
    }
}
fn terminal(page: &Page) -> bool {
    page.data.len() <= 1
        && page.data.iter().all(|t| {
            !t.id.is_empty()
                && t.id.len() <= 256
                && !t.id.chars().any(char::is_control)
                && matches!(t.status.as_str(), "completed" | "failed" | "interrupted")
        })
}

#[cfg(test)]
mod tests;
