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
        let history = std::fs::canonicalize(history?).ok()?;
        let home = std::fs::canonicalize(&a.home).ok()?;
        if !history.starts_with(home.join("sessions"))
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
mod tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[tokio::test]
    async fn concurrent_observers_share_one_reconstruction_and_file_races_fail_closed() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let history = a.home.join("sessions/fixture.jsonl");
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(&history, "synthetic").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let changed = Arc::new(AtomicUsize::new(0));
        let (count, mutate, path, id) = (
            calls.clone(),
            changed.clone(),
            history.clone(),
            a.id.clone(),
        );
        let listener = tokio::net::UnixListener::bind(a.endpoint()).unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (count, mutate, path, id) =
                    (count.clone(), mutate.clone(), path.clone(), id.clone());
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let r: Value = serde_json::from_str(&text).unwrap();
                        let result = match r["method"].as_str().unwrap() {
                            "initialized" => continue,
                            "initialize" => json!({"userAgent":"fixture/0.163.0"}),
                            "thread/read" => {
                                json!({"thread":{"id":id,"path":path,"status":{"type":"idle"}}})
                            }
                            "thread/turns/list" => {
                                count.fetch_add(1, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                if mutate.load(Ordering::SeqCst) == 1 {
                                    std::fs::write(&path, "changed while reading").unwrap();
                                }
                                json!({"data":[{"id":"last-turn","status":"completed"}]})
                            }
                            _ => panic!("unexpected RPC"),
                        };
                        if ws
                            .send(Message::Text(
                                json!({"id":r["id"],"result":result}).to_string().into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        let mut one = a.connect().await.unwrap();
        let mut two = a.connect().await.unwrap();
        let (first, second) = tokio::join!(
            super::super::observe_cached(&mut one, &paths, &a),
            super::super::observe_cached(&mut two, &paths, &a)
        );
        assert_eq!(first.unwrap().state, "idle");
        assert_eq!(second.unwrap().state, "idle");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        changed.store(1, Ordering::SeqCst);
        std::fs::write(&history, "invalidate old proof").unwrap();
        let err = super::super::observe_cached(&mut one, &paths, &a)
            .await
            .err()
            .unwrap();
        assert_eq!(err.0, "history_changed_during_observation");
        task.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_summary_is_amortized_but_fresh_status_and_changed_history_are_not_cached() {
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let history = a.home.join("sessions/fixture.jsonl");
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(&history, "synthetic history").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let busy = Arc::new(AtomicUsize::new(0));
        let (count, checked, working) = (calls.clone(), reads.clone(), busy.clone());
        let path = history.clone();
        let id = a.id.clone();
        let server = scripted_server(&a.endpoint(), move |r| {
            match r["method"].as_str().unwrap() {
                "thread/read" => {
                    checked.fetch_add(1, Ordering::SeqCst);
                    Some(json!({"thread":{"id":id,"path":path,"status":{"type":if working.load(Ordering::SeqCst)==1 {"active"} else {"idle"}}}}))
                }
                "thread/turns/list" => {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        std::thread::sleep(Duration::from_millis(3250));
                    }
                    Some(json!({"data":[{"id":"last-turn","status":"completed"}],"nextCursor":null}))
                }
                _ => panic!("Unexpected RPC: {}",r["method"]),
            }
        }).await;
        let mut control = a.connect().await.unwrap();
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            super::super::observe_cached(&mut control, &paths, &a),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.state, "idle");
        let first_at = first.summary_evidence.unwrap()["observed_at"].clone();
        for _ in 0..5 {
            let o = super::super::observe_cached(&mut control, &paths, &a)
                .await
                .unwrap();
            assert_eq!(o.state, "idle");
            let evidence = o.summary_evidence.unwrap();
            assert_eq!(evidence["source"], "unchanged_history_cache");
            assert_eq!(evidence["observed_at"], first_at);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 12);
        busy.store(1, Ordering::SeqCst);
        assert_eq!(
            super::super::observe_cached(&mut control, &paths, &a)
                .await
                .unwrap()
                .state,
            "working"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        busy.store(0, Ordering::SeqCst);
        std::fs::write(&history, "changed synthetic history").unwrap();
        super::super::observe_cached(&mut control, &paths, &a)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let mut replaced = a.clone();
        replaced.start += 1;
        super::super::observe_cached(&mut control, &paths, &replaced)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Unchanged disk data is not evidence about newly started in-memory
        // turns. Expiry must force native reconstruction, not renew old proof.
        let cache = Cache::new(&paths, &replaced, Some(&history)).unwrap();
        let mut saved: Saved =
            serde_json::from_slice(&std::fs::read(&cache.file).unwrap()).unwrap();
        saved.observed_at = crate::daemon::now() - 5;
        std::fs::write(&cache.file, serde_json::to_vec(&saved).unwrap()).unwrap();
        super::super::observe_cached(&mut control, &paths, &replaced)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        server.abort();
    }
}
