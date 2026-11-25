//! Read-only host placement observations, separate from lifecycle generations.
//! Private locator mapping never appears in the public projection.
use super::*;
use crate::{daemon, launch::Host};
use serde_json::{Value, json};

#[derive(Default, Serialize, Deserialize)]
struct Placements {
    hosts: BTreeMap<String, HostRecord>,
    sessions: BTreeMap<String, Evidence>,
}
#[derive(Serialize, Deserialize)]
struct HostRecord {
    id: String,
    label: String,
    spaces: BTreeMap<String, Space>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Space {
    id: String,
    label: String,
    observed_at: i64,
}
#[derive(Serialize, Deserialize)]
struct Evidence {
    attachment: Attachment,
    host_id: String,
    space: Space,
    observed_at: i64,
    checked_at: i64,
    reason: Option<String>,
}
fn file(paths: &Paths) -> std::path::PathBuf {
    paths
        .root
        .join(crate::layout::SESSION_CATALOG)
        .join("placements.json")
}
fn read(paths: &Paths) -> Result<Placements> {
    Ok(read_raw(paths)?.0)
}
/// The parsed file and its exact bytes (empty when absent).
fn read_raw(paths: &Paths) -> Result<(Placements, Vec<u8>)> {
    match fs::read(file(paths)) {
        Ok(bytes) => Ok((serde_json::from_slice(&bytes)?, bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Placements::default(), vec![])),
        Err(e) => Err(e.into()),
    }
}
fn unique<'a>(v: &'a Value, list: &str, key: &str, id: &str) -> Result<&'a Value> {
    let mut found = v[list]
        .as_array()
        .context("invalid_host_snapshot")?
        .iter()
        .filter(|v| v[key] == id);
    let value = found.next().context("membership_unobserved")?;
    ensure!(found.next().is_none(), "ambiguous_host_identity");
    Ok(value)
}
fn valid_spaces(snapshot: &Value) -> bool {
    let Some(spaces) = snapshot["workspaces"].as_array() else {
        return false;
    };
    let mut ids = std::collections::BTreeSet::new();
    spaces.iter().all(|s| {
        s["workspace_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && ids.insert(id))
            && s["label"]
                .as_str()
                .is_some_and(|label| !label.chars().any(char::is_control))
    })
}
fn membership<'a>(snapshot: &'a Value, a: &Attachment) -> Result<(&'a str, &'a str)> {
    let (_, id) = crate::terminal::target(&a.target)?;
    let pane = unique(snapshot, "panes", "pane_id", id)?;
    let space = pane["workspace_id"]
        .as_str()
        .context("membership_unobserved")?;
    let tab = unique(
        snapshot,
        "tabs",
        "tab_id",
        pane["tab_id"].as_str().context("membership_unobserved")?,
    )?;
    ensure!(tab["workspace_id"] == space, "inconsistent_host_snapshot");
    let workspace = unique(snapshot, "workspaces", "workspace_id", space)?;
    Ok((
        space,
        workspace["label"]
            .as_str()
            .context("membership_unobserved")?,
    ))
}
/// Called with the same host snapshot used by name sync. Only verified live
/// inventory attachments can introduce evidence; receipts are not required.
type HostObservation = (Session, Host, Option<(i64, Value)>);
pub(crate) fn record(paths: &Paths, observations: Vec<HostObservation>) -> Result<()> {
    transaction(paths, |catalog| {
        let (mut data, current) = read_raw(paths)?;
        let now = daemon::now();
        for (session, host, snapshot) in observations {
            let Some(a) = session.attachment.as_ref() else {
                continue;
            };
            if catalog
                .sessions
                .get(&session.id)
                .is_none_or(|s| s.attachment.as_ref() != Some(a))
            {
                continue;
            }
            let key = serde_json::to_string(&(host.session.clone(), host.socket))?;
            let h = data.hosts.entry(key).or_insert_with(|| HostRecord {
                id: uuid::Uuid::new_v4().to_string(),
                label: host.session,
                spaces: BTreeMap::new(),
            });
            let snapshot = snapshot.filter(|(_, value)| valid_spaces(value));
            let observed_at = snapshot.as_ref().map(|(at, _)| *at).unwrap_or(now);
            if let Some(list) = snapshot
                .as_ref()
                .and_then(|(_, s)| s["workspaces"].as_array())
            {
                for workspace in list {
                    if let (Some(id), Some(label)) = (
                        workspace["workspace_id"].as_str(),
                        workspace["label"].as_str(),
                    ) {
                        let space = h.spaces.entry(id.into()).or_insert_with(|| Space {
                            id: uuid::Uuid::new_v4().to_string(),
                            label: label.into(),
                            observed_at,
                        });
                        space.label = label.into();
                        space.observed_at = observed_at;
                    }
                }
            }
            let result = snapshot
                .as_ref()
                .context("host_unobserved")
                .and_then(|(_, s)| membership(s, a));
            match result {
                Ok((native, label)) => {
                    let space = h.spaces.entry(native.into()).or_insert_with(|| Space {
                        id: uuid::Uuid::new_v4().to_string(),
                        label: label.into(),
                        observed_at,
                    });
                    space.label = label.into();
                    space.observed_at = observed_at;
                    data.sessions.insert(
                        session.id,
                        Evidence {
                            attachment: a.clone(),
                            host_id: h.id.clone(),
                            space: space.clone(),
                            observed_at,
                            checked_at: now,
                            reason: None,
                        },
                    );
                }
                Err(e) => {
                    if let Some(old) = data.sessions.get_mut(&session.id) {
                        old.checked_at = now;
                        old.reason = Some(e.to_string());
                    }
                }
            }
        }
        // Nothing to record is not a write (no rename, no fsync): the file is
        // replaced only when its content changes.
        let next = serde_json::to_vec(&data)?;
        if next == current {
            return Ok(());
        }
        atomic_write(&file(paths), &next)
    })
}
/// One disk read per public batch. Fresh means bounded host evidence, never an
/// agent-readiness assertion. Historical placement does not authorize a move.
pub fn project(paths: &Paths, sessions: &[Session]) -> Result<Value> {
    let data = read(paths)?;
    let now = daemon::now();
    let memberships: BTreeMap<_,_>=sessions.iter().map(|s| {
        let mut value=if let Some(e)=data.sessions.get(&s.id) {
            let reason=if s.attachment.is_none(){Some("session_unattached")}
                else if s.attachment.as_ref()!=Some(&e.attachment){Some("attachment_changed")}
                else if let Some(r)=e.reason.as_deref(){Some(r)}
                else if now.saturating_sub(e.observed_at)>30 {Some("observation_expired")} else {None};
            json!({"version":1,"state":if reason.is_none(){"observed"}else{"last_known"},"host_id":e.host_id,"space":{"id":e.space.id,"label":e.space.label},"observed_at":e.observed_at,"valid_until":e.observed_at+30,"checked_at":e.checked_at,"reason":reason,"provenance":"herdr_snapshot_verified_catalog_attachment"})
        } else {json!({"version":1,"state":"unobserved","host_id":null,"space":null,"observed_at":null,"valid_until":null,"checked_at":null,"reason":if s.attachment.as_ref().is_some_and(|a|a.host!="herdr"){"not_herdr"}else{"no_verified_placement"},"provenance":null})};
        value["catalog_generation"]=json!(s.generation());
        (s.id.clone(),value)
    }).collect();
    let hosts:Vec<_>=data.hosts.values().map(|h|json!({"id":h.id,"kind":"herdr","label":h.label,"spaces":h.spaces.values().map(|s|json!({"id":s.id,"label":s.label,"observed_at":s.observed_at})).collect::<Vec<_>>()})).collect();
    Ok(
        json!({"version":1,"observed_at":now,"hosts":hosts,"memberships":memberships,"starts_process":false,"sends_input":false}),
    )
}

/// Catalog ids that host placement evidence was recorded for (ids only).
pub(crate) fn evidence_ids(paths: &Paths) -> Result<Vec<String>> {
    Ok(read(paths)?.sessions.into_keys().collect())
}

#[cfg(test)]
mod tests;
