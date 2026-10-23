//! Resumable read-only summary scan. Progress is not delivery evidence.
use super::*;
use std::{os::unix::fs::MetadataExt, path::Path};

#[derive(Serialize, Deserialize, PartialEq)]
struct Revision {
    path: PathBuf,
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
fn revision(home: &Path, path: &Path) -> Option<Revision> {
    let path = fs::canonicalize(path).ok()?;
    let home = fs::canonicalize(home).ok()?;
    if !path.starts_with(home.join("sessions")) && !path.starts_with(home.join("archived_sessions"))
    {
        return None;
    }
    let m = fs::metadata(&path).ok()?;
    (m.is_file() && m.uid() == unsafe { libc::geteuid() }).then(|| Revision {
        path,
        device: m.dev(),
        inode: m.ino(),
        bytes: m.len(),
        modified: (m.mtime(), m.mtime_nsec()),
        changed: (m.ctime(), m.ctime_nsec()),
    })
}
#[derive(Serialize, Deserialize)]
struct Progress {
    receipt_hash: String,
    generation: String,
    revision: Revision,
    started_at: i64,
    head: String,
    cursor: Option<String>,
    cursors: HashSet<String>,
    pages: usize,
    candidate: Option<Receipt>,
}
fn hash(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|_| Fault("storage_error"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
async fn page(control: &mut Transport, id: &str, cursor: Option<&str>) -> Result<Page> {
    let p: Page = control.passive_summary_page(id, cursor, 50).await?;
    if p.data.len() > 50 {
        return Err(Fault("malformed_summary"));
    }
    Ok(p)
}
fn discard(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Fault("storage_error")),
    }
}
pub(super) async fn read(
    paths: &Paths,
    a: &Attachment,
    control: &mut Transport,
    receipt: &Receipt,
) -> Result<Option<Receipt>> {
    #[derive(Deserialize)]
    struct Read {
        thread: Thread,
    }
    #[derive(Deserialize)]
    struct Thread {
        id: String,
        path: Option<PathBuf>,
    }
    let native: Read = control
        .request("thread/read", json!({"threadId":a.id,"includeTurns":false}))
        .await?;
    if native.thread.id != a.id {
        return Err(Fault("correlation_changed"));
    }
    let Some(rev) = native
        .thread
        .path
        .as_deref()
        .and_then(|p| revision(&a.home, p))
    else {
        return super::scan(control, receipt, &a.generation()).await;
    };
    let directory = paths.root.join("messaging/observations");
    private_dir(&directory).map_err(|_| Fault("storage_error"))?;
    let file = directory.join(format!("{}.json", receipt.message_id));
    let lock = lock_file(&file.with_extension("lock")).map_err(|_| Fault("storage_error"))?;
    lock.try_lock_exclusive()
        .map_err(|_| Fault("reply_observation_busy"))?;
    let generation = a.generation();
    let receipt_hash = hash(receipt)?;
    let now = crate::daemon::now();
    let old = fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Progress>(&b).ok())
        .filter(|p| {
            p.receipt_hash == receipt_hash
                && p.generation == generation
                && p.revision == rev
                && p.started_at <= now
                && now < p.started_at + 300
                && p.pages <= 20
        });
    let mut progress = old.unwrap_or(Progress {
        receipt_hash,
        generation: generation.clone(),
        revision: rev,
        started_at: now,
        head: String::new(),
        cursor: None,
        cursors: HashSet::new(),
        pages: 0,
        candidate: None,
    });
    let verify_head = progress.pages > 0 && progress.cursor.is_none();
    let p = page(control, &a.id, progress.cursor.as_deref()).await?;
    if revision(&a.home, &progress.revision.path).as_ref() != Some(&progress.revision) {
        discard(&file)?;
        return Err(Fault("history_changed_during_observation"));
    }
    let digest = hash(&p)?;
    if verify_head {
        discard(&file)?;
        if digest != progress.head {
            return Err(Fault("reply_scan_changed"));
        }
        return Ok(progress.candidate);
    }
    if progress.pages == 0 {
        progress.head = digest;
    }
    consume(p.data, receipt, &generation, &mut progress.candidate)?;
    progress.pages += 1;
    progress.cursor = p.next;
    if let Some(next) = &progress.cursor {
        if next.len() > 4096 || !progress.cursors.insert(next.clone()) {
            discard(&file)?;
            return Err(Fault("reply_scan_incomplete"));
        }
        if progress.pages >= 20 {
            discard(&file)?;
            return Err(Fault("reply_scan_limit_exceeded"));
        }
    } else if progress.pages == 1 {
        discard(&file)?;
        return Ok(progress.candidate);
    }
    crate::paths::atomic_write(
        &file,
        &serde_json::to_vec(&progress).map_err(|_| Fault("storage_error"))?,
    )
    .map_err(|_| Fault("storage_error"))?;
    Err(Fault("reply_scan_pending"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pages_resume_without_partial_claim_and_revalidate_live_head() {
        let (_tmp, paths, input, now) = fixture();
        let a = attachment_fixture(&paths, &input);
        let receipt = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
        let history = a.home.join("sessions/test.jsonl");
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        fs::write(&history, "synthetic").unwrap();
        let phase = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let (mode, count, path, id, client) = (
            phase.clone(),
            reads.clone(),
            history.clone(),
            a.id.clone(),
            receipt.message_id.clone(),
        );
        let server = scripted_server(&a.endpoint(),move |r| {
            if r["method"] == "thread/read" {
                return Some(json!({"thread":{"id":id,"path":path}}));
            }
            assert_eq!(r["method"],"thread/turns/list");
            if count.fetch_add(1,Ordering::SeqCst)==0 { std::thread::sleep(Duration::from_millis(3250)); }
            let turn = json!({"id":"turn-1","status":"completed","itemsView":"summary","items":[
                {"type":"userMessage","id":"item-1","clientId":client,"content":"SECRET_NOT_STORED"},
                {"type":"agentMessage","id":"reply-1","text":"Visible answer","phase":"final_answer"}]});
            Some(if r["params"]["cursor"].is_null() {
                json!({"data":if mode.load(Ordering::SeqCst)==1 {json!([])}else{json!([turn])},"nextCursor":"older"})
            } else { json!({"data":[],"nextCursor":null}) })
        }).await;
        let mut c = a.connect().await.unwrap();
        for _ in 0..2 {
            assert_eq!(
                read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
                "reply_scan_pending"
            );
        }
        // The completed candidate is still private until older pages and the
        // live head agree. A changed in-memory projection invalidates it.
        phase.store(1, Ordering::SeqCst);
        assert_eq!(
            read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
            "reply_scan_changed"
        );
        phase.store(0, Ordering::SeqCst);
        for _ in 0..2 {
            assert_eq!(
                read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
                "reply_scan_pending"
            );
        }
        let file = paths
            .root
            .join("messaging/observations")
            .join(format!("{}.json", receipt.message_id));
        assert!(
            !fs::read_to_string(&file)
                .unwrap()
                .contains("SECRET_NOT_STORED")
        );
        let found = read(&paths, &a, &mut c, &receipt).await.unwrap().unwrap();
        assert_eq!(found.state, "completed_with_reply");
        assert_eq!(reads.load(Ordering::SeqCst), 6);
        assert!(!file.exists());
        server.abort();
    }
    #[tokio::test]
    async fn duplicate_across_polls_is_rejected_and_disk_change_resets_scan() {
        let (_tmp, paths, input, now) = fixture();
        let a = attachment_fixture(&paths, &input);
        let receipt = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
        let history = a.home.join("sessions/test.jsonl");
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        fs::write(&history, "synthetic").unwrap();
        let cursors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (seen, path, id, client) = (
            cursors.clone(),
            history.clone(),
            a.id.clone(),
            receipt.message_id.clone(),
        );
        let server = scripted_server(&a.endpoint(),move |r| {
            if r["method"]=="thread/read" { return Some(json!({"thread":{"id":id,"path":path}})); }
            assert_eq!(r["method"],"thread/turns/list");
            seen.lock().unwrap().push(r["params"]["cursor"].clone());
            Some(json!({"data":[{"id":"turn-1","status":"inProgress","itemsView":"summary","items":[{"type":"userMessage","id":"item-1","clientId":client}]}],"nextCursor":if r["params"]["cursor"].is_null(){json!("older")}else{Value::Null}}))
        }).await;
        let mut c = a.connect().await.unwrap();
        assert_eq!(
            read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
            "reply_scan_pending"
        );
        fs::write(&history, "changed history").unwrap();
        assert_eq!(
            read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
            "reply_scan_pending"
        );
        assert_eq!(
            read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
            "ambiguous_native_correlation"
        );
        assert_eq!(
            *cursors.lock().unwrap(),
            vec![Value::Null, Value::Null, json!("older")]
        );
        assert!(receipt.correlation.is_none());
        server.abort();
    }
}
