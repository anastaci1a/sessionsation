//! HERDR owns live tab labels. The catalog retains the last observation for history.
use super::*;
use crate::{
    external,
    launch::{Host, host_call},
};
use serde_json::Value;
use std::time::Duration;

fn lock(paths: &Paths) -> Result<fs::File> {
    let dir = paths.root.join("session-catalog");
    private_dir(&dir)?;
    let lock = lock_file(&dir.join("names.lock"))?;
    lock.try_lock_exclusive().context("NAME_SYNC_BUSY")?;
    Ok(lock)
}
fn host(paths: &Paths, s: &Session) -> Result<Host> {
    let a = s.attachment.as_ref().context("NO_LIVE_ATTACHMENT")?;
    ensure!(a.host == "herdr", "SESSION_NOT_IN_HERDR");
    ensure!(
        !external::attachment_definitely_gone(a.pid, a.process_start),
        "STALE_TARGET"
    );
    let (server, _) = crate::terminal::target(&a.target)?;
    Ok(Host {
        session: server.into(),
        socket: external::hosting::socket_for(paths, &a.target)?
            .context("HOST_SOCKET_UNAVAILABLE")?,
    })
}
async fn snapshot(host: &Host) -> Result<Value> {
    // One bounded snapshot per server, never per session/provider history read.
    Ok(tokio::time::timeout(
        Duration::from_secs(1),
        host_call(host, &["api", "snapshot"]),
    )
    .await
    .context("HOST_NAME_UNOBSERVED")??["snapshot"]
        .clone())
}
fn tab<'a>(snapshot: &'a Value, target: &str) -> Result<(&'a str, &'a str)> {
    let (_, pane_id) = crate::terminal::target(target)?;
    let panes = snapshot["panes"]
        .as_array()
        .context("HOST_NAME_UNOBSERVED")?;
    let mut matches = panes.iter().filter(|p| p["pane_id"] == pane_id);
    let pane = matches.next().context("HOST_PANE_UNOBSERVED")?;
    ensure!(matches.next().is_none(), "AMBIGUOUS_HOST_PANE");
    let id = pane["tab_id"].as_str().context("HOST_TAB_UNOBSERVED")?;
    let tabs = snapshot["tabs"]
        .as_array()
        .context("HOST_NAME_UNOBSERVED")?;
    let mut matches = tabs.iter().filter(|t| t["tab_id"] == id);
    let tab = matches.next().context("HOST_TAB_UNOBSERVED")?;
    ensure!(matches.next().is_none(), "AMBIGUOUS_HOST_TAB");
    let label = tab["label"].as_str().context("HOST_NAME_UNOBSERVED")?;
    ensure!(
        !label.is_empty() && !label.chars().any(char::is_control),
        "INVALID_HOST_NAME"
    );
    Ok((id, label))
}
fn apply(paths: &Paths, observations: Vec<(Session, String)>) -> Result<()> {
    transaction(paths, |c| {
        for (before, name) in observations {
            if let Some(current) = c.sessions.get_mut(&before.id)
                && current.generation() == before.generation()
                && current.name != name
            {
                // Host labels are presentation, not unique identities. Never invent a
                // suffix or truncate a host-owned label, even for split-pane sessions.
                current.name = name;
                current.revision += 1;
            }
        }
        Ok(())
    })
}
pub async fn refresh(paths: &Paths, instances: &[external::Instance]) -> Result<()> {
    let Ok(_lock) = lock(paths) else {
        return Ok(());
    };
    let mut snapshots = BTreeMap::new();
    let mut observations = vec![];
    for s in list(paths)? {
        let Some(a) = &s.attachment else {
            continue;
        };
        if a.host != "herdr" || !instances.iter().any(|i| crate::api::source_matches(&s, i)) {
            continue;
        }
        let Ok(host) = host(paths, &s) else {
            continue;
        };
        let key = (host.session.clone(), host.socket.clone());
        if !snapshots.contains_key(&key) {
            snapshots.insert(key.clone(), snapshot(&host).await.ok());
        }
        if let Some(Some(value)) = snapshots.get(&key)
            && let Ok((_, label)) = tab(value, &a.target)
        {
            observations.push((s.clone(), label.into()));
        }
    }
    apply(paths, observations)
}
pub async fn rename(paths: &Paths, id: &str, generation: &str, name: &str) -> Result<Session> {
    validate_name(name)?;
    let _lock = lock(paths)?;
    let s = resolve(paths, id, generation)?;
    if s.attachment.as_ref().is_none_or(|a| a.host != "herdr") {
        return super::rename(paths, id, generation, name);
    }
    let _operation =
        external::messaging_lock(paths, &s.conversation_id).map_err(|e| anyhow::anyhow!(e))?;
    let host = host(paths, &s)?;
    let target = &s.attachment.as_ref().unwrap().target;
    let before = snapshot(&host).await?;
    let (tab_id, _) = tab(&before, target)?;
    let (_, pane_id) = crate::terminal::target(target)?;
    let process = tokio::time::timeout(
        Duration::from_secs(1),
        host_call(&host, &["pane", "process-info", "--pane", pane_id]),
    )
    .await
    .context("HOST_PROCESS_UNOBSERVED")??;
    let processes = process["process_info"]["foreground_processes"]
        .as_array()
        .context("HOST_PROCESS_UNOBSERVED")?;
    ensure!(
        processes.len() == 1
            && processes[0]["pid"].as_u64() == Some(s.attachment.as_ref().unwrap().pid as u64),
        "STALE_TARGET"
    );
    self::host(paths, &s)?;
    // The catalog remains only a cached label for live HERDR sessions. A tab may
    // contain several sessions; all acquire the host label on their next refresh.
    resolve(paths, id, generation)?;
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        host_call(&host, &["tab", "rename", tab_id, name]),
    )
    .await;
    let after = snapshot(&host)
        .await
        .context("HOST_RENAME_UNCONFIRMED: refresh session names before retrying")?;
    let (observed_tab, label) = tab(&after, target)?;
    ensure!(observed_tab == tab_id, "STALE_TARGET");
    // Always retain authoritative host evidence, including a concurrent host edit.
    apply(paths, vec![(s.clone(), label.into())])?;
    ensure!(
        label == name,
        "HOST_RENAME_NOT_APPLIED: refresh session names before retrying"
    );
    // A matching read-back reconciles a lost response; it never replays a mutation.
    drop(result);
    let current = list(paths)?
        .into_iter()
        .find(|v| v.id == id)
        .context("SESSION_NOT_FOUND")?;
    ensure!(
        current.attachment == s.attachment && current.name == name,
        "STALE_TARGET"
    );
    Ok(current)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_labels_are_identity_mapped_and_not_generated_from_titles() {
        let v = serde_json::json!({"panes":[{"pane_id":"w1:p1","tab_id":"w1:t2"}],"tabs":[{"tab_id":"w1:t1","label":"wrong"},{"tab_id":"w1:t2","label":"host name"}]});
        assert_eq!(tab(&v, "herdr/test/w1:p1").unwrap(), ("w1:t2", "host name"));
        assert!(tab(&v, "herdr/test/w1:p2").is_err());
    }
    #[test]
    fn host_duplicates_remain_labels_and_unchanged_reads_do_not_change_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let a = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "one",
            None,
        )
        .unwrap();
        let b = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "two",
            None,
        )
        .unwrap();
        apply(&p, vec![(a, "same".into()), (b, "same".into())]).unwrap();
        let before = list(&p).unwrap();
        assert_ne!(before[0].id, before[1].id);
        assert!(before.iter().all(|s| s.name == "same"));
        assert_eq!(
            super::super::rename(&p, &before[0].id, &before[0].generation(), "same")
                .unwrap()
                .generation(),
            before[0].generation()
        );
        apply(
            &p,
            before.iter().cloned().map(|s| (s, "same".into())).collect(),
        )
        .unwrap();
        assert_eq!(
            before.iter().map(Session::generation).collect::<Vec<_>>(),
            list(&p)
                .unwrap()
                .iter()
                .map(Session::generation)
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn observed_labels_survive_exit_but_stale_observations_never_undo_a_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let s = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "old",
            None,
        )
        .unwrap();
        apply(&p, vec![(s.clone(), "HERDR label".into())]).unwrap();
        let current = list(&p).unwrap().remove(0);
        assert_eq!(current.id, s.id);
        assert_eq!(current.conversation_id, s.conversation_id);
        assert_eq!(current.name, "HERDR label");
        apply(&p, vec![(s, "stale".into())]).unwrap();
        assert_eq!(list(&p).unwrap()[0].name, "HERDR label");
        assert_eq!(reconcile(&p, &[]).unwrap()[0].name, "HERDR label");
    }
}
