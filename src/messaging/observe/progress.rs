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
    let path = crate::alias::canonical(path).ok()?;
    let home = crate::alias::canonical(home).ok()?;
    // raw-path: both sides are alias::canonical forms (above)
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
mod tests;
