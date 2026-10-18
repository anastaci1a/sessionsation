//! Verify native history reconstruction without rewriting rollouts or copying databases.
use super::*;
use crate::control::Control;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryCheck {
    // Version 0 is the original exact-wire hash, retained for in-flight jobs.
    #[serde(default)]
    pub version: u8,
    pub thread_id: String,
    pub turns: usize,
    pub items: usize,
    pub digest: String,
}

fn canonical_entry(entry: &Value, items: bool, version: u8) -> Value {
    let mut entry = entry.clone();
    if items && version == 1 {
        if let Some(object) = entry.as_object_mut() {
            // Native reconstruction can populate these optional producer timings
            // when the source index returned null. Keep the entire nested item,
            // turn identity, ordering, and all other metadata byte-for-byte.
            object.remove("startedAtMs");
            object.remove("completedAtMs");
        }
    }
    entry
}

async fn fingerprint(control: &mut Control, id: &str, version: u8) -> Result<HistoryCheck> {
    ensure!(version <= 1, "unsupported history verification version");
    let mut hash = Sha256::new();
    let mut counts = [0, 0];
    for (index, method) in ["thread/turns/list", "thread/items/list"]
        .iter()
        .enumerate()
    {
        hash.update(method.as_bytes());
        let mut cursor = Value::Null;
        let mut seen = std::collections::HashSet::new();
        loop {
            let mut params =
                json!({"threadId":id,"limit":64,"sortDirection":"asc","cursor":cursor});
            if index == 0 {
                params["itemsView"] = json!("notLoaded");
            }
            let page = control
                .request_timeout(method, params, Duration::from_secs(60))
                .await?;
            let data = page["data"]
                .as_array()
                .context("Codex did not return a history page")?;
            for entry in data {
                // serde_json's ordered maps make this independent of wire key ordering.
                let bytes = serde_json::to_vec(&canonical_entry(entry, index == 1, version))?;
                hash.update((bytes.len() as u64).to_be_bytes());
                hash.update(bytes);
                counts[index] += 1;
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
            ensure!(
                !data.is_empty() && cursor.is_string() && seen.insert(cursor.to_string()),
                "Codex returned inconsistent history pagination"
            );
        }
    }
    Ok(HistoryCheck {
        version,
        thread_id: id.into(),
        turns: counts[0],
        items: counts[1],
        digest: format!("{:x}", hash.finalize()),
    })
}

pub(super) async fn capture(paths: &Paths, job: &mut Job, files: &[PathBuf]) -> Result<()> {
    let mut control = if crate::control::available(&job.source.home) {
        Some(Control::connect(&job.source.home).await?)
    } else {
        None
    };
    job.history_checks.clear();
    for file in files {
        let meta = session_meta(file)?;
        let id = meta["id"].as_str().context("missing history identity")?;
        let paginated = if let Some(control) = control.as_mut() {
            control.thread(id, false).await?["historyMode"] == "paginated"
                || meta["history_mode"] == "paginated"
        } else {
            meta["history_mode"] == "paginated"
        };
        if !paginated {
            continue;
        }
        movement::phase(
            paths,
            job,
            "checking_history",
            "Checking the full conversation before transfer. No export is needed; the original remains available.",
        )?;
        let control = control.as_mut().context("History verification cannot connect to the source Codex server. Open this conversation from Sessions, then retry the switch here.")?;
        let check = fingerprint(control, id, 1).await.context("Cannot verify the source history yet. The source is unchanged. Open it from Sessions, then choose Retry in Activity")?;
        job.history_checks.push(check);
    }
    save_job(paths, job)
}

pub(super) async fn verify(control: &mut Control, expected: &HistoryCheck) -> Result<()> {
    let actual = fingerprint(control, &expected.thread_id, expected.version).await.context("Could not verify the transferred history. The switcher will attempt to restore the source; use Retry in Activity after checking the job details")?;
    let mismatch = if actual.turns == expected.turns && actual.items == expected.items {
        "Turn and item totals match, but their content or order differs."
    } else {
        "The number of turns or items differs."
    };
    ensure!(
        &actual == expected,
        "Transferred history did not match the original (expected {} turns / {} items; found {} / {}). {} The switcher will attempt to restore the source. Use Retry in Activity; no manual export is needed",
        expected.turns,
        expected.items,
        actual.turns,
        actual.items,
        mismatch
    );
    Ok(())
}

pub(super) async fn prepare_children(
    paths: &Paths,
    job: &mut Job,
    target: &mut Control,
    destinations: &std::collections::BTreeMap<String, PathBuf>,
) -> Result<()> {
    for check in job.history_checks.clone() {
        if check.thread_id == job.source.thread_id {
            continue;
        }
        let rollout = destinations
            .get(&check.thread_id)
            .context("copied child history is missing")?;
        job.prepared_threads.push(check.thread_id.clone());
        save_job(paths, job)?;
        let (approval, sandbox) = recorded_permissions(rollout)
            .unwrap_or_else(|| ("never".into(), "danger-full-access".into()));
        if let Err(error) = target
            .resume_with_permissions(
                json!({"threadId":check.thread_id,"path":rollout,"excludeTurns":true,"approvalPolicy":approval,"sandbox":sandbox}),
            )
            .await {
            if error.to_string().starts_with("Codex rejected") {
                job.prepared_threads.retain(|id| id != &check.thread_id);
                save_job(paths, job)?;
            }
            return Err(error);
        }
        verify(target, &check).await?;
        target.verify_unloadable(&check.thread_id, false).await?;
        // Build the native index, then leave children saved and idle until resumed.
        target
            .request("thread/archive", json!({"threadId":check.thread_id}))
            .await?;
        target
            .request("thread/unarchive", json!({"threadId":check.thread_id}))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_normalization_only_excludes_item_envelope_timings() {
        let source = json!({"turnId":"turn-1","startedAtMs":null,"completedAtMs":null,
            "item":{"id":"item-1","type":"agentMessage","text":"original","startedAtMs":123}});
        let mut rebuilt = source.clone();
        rebuilt["startedAtMs"] = json!(1000);
        rebuilt["completedAtMs"] = json!(2000);
        assert_eq!(
            canonical_entry(&source, true, 1),
            canonical_entry(&rebuilt, true, 1)
        );
        assert_ne!(
            canonical_entry(&source, true, 0),
            canonical_entry(&rebuilt, true, 0)
        );
        assert_ne!(
            canonical_entry(&source, false, 1),
            canonical_entry(&rebuilt, false, 1)
        );
        for pointer in ["/turnId", "/item/id", "/item/text", "/item/startedAtMs"] {
            let mut changed = rebuilt.clone();
            *changed.pointer_mut(pointer).unwrap() = json!("changed");
            assert_ne!(
                canonical_entry(&source, true, 1),
                canonical_entry(&changed, true, 1)
            );
        }
        let legacy: HistoryCheck = serde_json::from_value(json!({
            "thread_id":"thread-1","turns":1,"items":1,"digest":"legacy"
        }))
        .unwrap();
        assert_eq!(legacy.version, 0);
    }
}
