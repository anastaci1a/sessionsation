//! Stable backend identity, distinct from native conversations and attachments.
pub mod names;
pub mod placement;
use crate::paths::{Paths, atomic_write, lock_file, private_dir};
use crate::provider::ProviderId;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub host: String,
    pub target: String,
    pub pid: u32,
    pub process_start: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub account: String,
    pub conversation_id: String,
    pub revision: u64,
    pub attachment: Option<Attachment>,
    /// Recorded working directory for providers that resume by directory
    /// (Claude). Omitted for Codex, whose generations are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Native conversations this entry continued from (oldest first), linked
    /// only through a provider-recorded continuation marker. Omitted when empty
    /// so existing generations are unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_conversation_ids: Vec<String>,
}

impl Session {
    pub fn generation(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).expect("serializable session"))
        )
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    schema_version: u32,
    sessions: BTreeMap<String, Session>,
    #[serde(default)]
    reservations: BTreeMap<String, String>,
    /// Catalog ids merged into another entry by a verified continuation link.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    aliases: BTreeMap<String, String>,
}

fn transaction<T>(paths: &Paths, change: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
    let dir = paths.root.join("session-catalog");
    private_dir(&dir)?;
    let lock = lock_file(&dir.join("catalog.lock"))?;
    lock.lock_exclusive()?;
    let file = dir.join("catalog.json");
    let mut catalog = if file.exists() {
        let c: Catalog = serde_json::from_slice(&fs::read(&file)?)?;
        ensure!(c.schema_version == 1, "UNSUPPORTED_CATALOG_VERSION");
        c
    } else {
        Catalog {
            schema_version: 1,
            ..Default::default()
        }
    };
    let before = serde_json::to_vec(&catalog)?;
    let result = change(&mut catalog)?;
    let after = serde_json::to_vec(&catalog)?;
    if before != after || !file.exists() {
        atomic_write(&file, &after)?;
    }
    Ok(result)
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name == name.trim()
            && name.chars().count() <= crate::model::SESSION_NAME_MAX
            && !name.chars().any(char::is_control),
        "INVALID_SESSION_NAME"
    );
    Ok(())
}

pub fn list(paths: &Paths) -> Result<Vec<Session>> {
    transaction(paths, |c| Ok(c.sessions.values().cloned().collect()))
}

pub fn record_launch(
    paths: &Paths,
    account: &str,
    conversation: &str,
    label: &str,
    attachment: Option<Attachment>,
) -> Result<Session> {
    record_provider_launch(
        paths,
        ProviderId::Codex,
        account,
        conversation,
        label,
        None,
        attachment,
    )
}

pub fn record_provider_launch(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
    conversation: &str,
    label: &str,
    directory: Option<&str>,
    attachment: Option<Attachment>,
) -> Result<Session> {
    transaction(paths, |c| {
        if let Some(s) = c
            .sessions
            .values_mut()
            .find(|s| s.provider == provider.as_str() && s.conversation_id == conversation)
        {
            if s.account != account || s.attachment != attachment {
                s.account = account.into();
                s.attachment = attachment;
                s.revision += 1;
            }
            return Ok(s.clone());
        }
        let base = label
            .chars()
            .filter(|c| !c.is_control())
            .take(crate::model::SESSION_NAME_MAX)
            .collect::<String>();
        let base = base.trim();
        let base = if base.is_empty() { "session" } else { base };
        let mut name = base.to_owned();
        let mut suffix = 2;
        while c.sessions.values().any(|s| s.name == name) || c.reservations.contains_key(&name) {
            let tail = format!("-{suffix}");
            let prefix: String = base
                .chars()
                .take(crate::model::SESSION_NAME_MAX.saturating_sub(tail.len()))
                .collect();
            name = format!("{prefix}{tail}");
            suffix += 1;
        }
        let s = Session {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            provider: provider.as_str().into(),
            account: account.into(),
            conversation_id: conversation.into(),
            revision: 1,
            attachment,
            directory: directory.map(str::to_owned),
            previous_conversation_ids: vec![],
        };
        c.sessions.insert(s.id.clone(), s.clone());
        Ok(s)
    })
}

/// A provider conversation found on disk (no process implied).
#[derive(Clone, Debug)]
pub struct Discovered {
    pub account: String,
    pub conversation_id: String,
    pub label: String,
    pub directory: Option<String>,
}
/// Register discovered conversations of one provider without changing existing
/// entries; returns every catalog session of that provider.
pub fn register_discovered(
    paths: &Paths,
    provider: ProviderId,
    discovered: &[Discovered],
) -> Result<Vec<Session>> {
    transaction(paths, |c| {
        for d in discovered {
            if c.sessions.values().any(|s| {
                s.provider == provider.as_str()
                    && (s.conversation_id == d.conversation_id
                        || s.previous_conversation_ids.contains(&d.conversation_id))
            }) {
                continue;
            }
            let base = d
                .label
                .chars()
                .filter(|c| !c.is_control())
                .take(crate::model::SESSION_NAME_MAX)
                .collect::<String>();
            let base = base.trim();
            let base = if base.is_empty() { "session" } else { base };
            let mut name = base.to_owned();
            let mut suffix = 2;
            while c.sessions.values().any(|s| s.name == name) || c.reservations.contains_key(&name)
            {
                let tail = format!("-{suffix}");
                let prefix: String = base
                    .chars()
                    .take(crate::model::SESSION_NAME_MAX.saturating_sub(tail.len()))
                    .collect();
                name = format!("{prefix}{tail}");
                suffix += 1;
            }
            let s = Session {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                provider: provider.as_str().into(),
                account: d.account.clone(),
                conversation_id: d.conversation_id.clone(),
                revision: 1,
                attachment: None,
                directory: d.directory.clone(),
                previous_conversation_ids: vec![],
            };
            c.sessions.insert(s.id.clone(), s);
        }
        Ok(c.sessions
            .values()
            .filter(|s| s.provider == provider.as_str())
            .cloned()
            .collect())
    })
}
/// Apply provider observations (attachment, first known directory) only to
/// entries that have not changed since they were observed.
pub fn observe_provider_sessions(
    paths: &Paths,
    observations: Vec<(Session, Option<Attachment>, Option<String>)>,
) -> Result<()> {
    transaction(paths, |c| {
        for (before, attachment, directory) in observations {
            let Some(current) = c.sessions.get_mut(&before.id) else {
                continue;
            };
            if current.generation() != before.generation() {
                continue;
            }
            let mut changed = false;
            if current.attachment != attachment {
                current.attachment = attachment;
                changed = true;
            }
            if current.directory.is_none() && directory.is_some() {
                current.directory = directory;
                changed = true;
            }
            if changed {
                current.revision += 1;
            }
        }
        Ok(())
    })
}

/// A provider-recorded continuation: `from` continued as `to` in one account.
#[derive(Clone, Debug, PartialEq)]
pub struct Continuation {
    pub account: String,
    pub from: String,
    pub to: String,
}
/// Link verified continuations: the entry of `from` keeps its stable id, name and
/// account and now follows `to` (`from` is retained in `previous_conversation_ids`).
/// An entry already registered for `to` is merged into it and left as an alias,
/// so one conversation lineage never has two catalog entries. Cross-account or
/// cross-provider links are never applied. Returns the merged (alias) ids.
pub fn link_continuations(
    paths: &Paths,
    provider: ProviderId,
    links: &[Continuation],
) -> Result<Vec<(String, String)>> {
    transaction(paths, |c| {
        let mut merged = vec![];
        // Chains (a→b→c) converge within the number of links.
        for _ in 0..=links.len() {
            let mut changed = false;
            for link in links {
                if link.from == link.to {
                    continue;
                }
                let Some(owner) = c
                    .sessions
                    .values()
                    .find(|s| s.provider == provider.as_str() && s.conversation_id == link.from)
                    .map(|s| s.id.clone())
                else {
                    continue;
                };
                if c.sessions[&owner].account != link.account {
                    continue;
                }
                let duplicate = c
                    .sessions
                    .values()
                    .find(|s| {
                        s.id != owner
                            && s.provider == provider.as_str()
                            && s.conversation_id == link.to
                    })
                    .cloned();
                if duplicate
                    .as_ref()
                    .is_some_and(|d| d.account != link.account)
                {
                    continue;
                }
                if let Some(d) = &duplicate {
                    c.sessions.remove(&d.id);
                    for target in c.aliases.values_mut() {
                        if *target == d.id {
                            *target = owner.clone();
                        }
                    }
                    c.aliases.insert(d.id.clone(), owner.clone());
                    merged.push((d.id.clone(), owner.clone()));
                }
                let s = c.sessions.get_mut(&owner).context("SESSION_NOT_FOUND")?;
                s.previous_conversation_ids.push(link.from.clone());
                if let Some(d) = duplicate {
                    for id in d.previous_conversation_ids {
                        if !s.previous_conversation_ids.contains(&id) {
                            s.previous_conversation_ids.push(id);
                        }
                    }
                    if d.attachment.is_some() {
                        s.attachment = d.attachment;
                    }
                    if d.directory.is_some() {
                        s.directory = d.directory;
                    }
                }
                s.conversation_id = link.to.clone();
                s.revision += 1;
                changed = true;
            }
            if !changed {
                break;
            }
        }
        Ok(merged)
    })
}

/// Register explicit discoveries; this records identity without moving a process.
pub fn reconcile(paths: &Paths, instances: &[crate::external::Instance]) -> Result<Vec<Session>> {
    transaction(paths, |c| {
        for i in instances {
            if i.thread_id.is_empty() || i.tmux_only {
                continue;
            }
            let key = c
                .sessions
                .values()
                .find(|s| s.provider == "codex" && s.conversation_id == i.thread_id)
                .map(|s| s.id.clone());
            let id = key.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let attachment = (!i.inactive).then(|| Attachment {
                host: if i.pane.starts_with("herdr/") {
                    "herdr"
                } else {
                    "tmux"
                }
                .into(),
                target: i.pane.clone(),
                pid: i.pid,
                process_start: i.start_time,
            });
            if let Some(s) = c.sessions.get_mut(&id) {
                // An unreadable host is an incomplete observation, not a stopped
                // agent. Saved copies cannot replace a still-live attachment.
                if i.inactive
                    && s.attachment.as_ref().is_some_and(|a| {
                        !crate::external::attachment_definitely_gone(a.pid, a.process_start)
                    })
                {
                    continue;
                }
                if s.account != i.account || s.attachment != attachment {
                    s.account = i.account.clone();
                    s.attachment = attachment;
                    s.revision += 1;
                }
            } else {
                let base = if !i.display_name.is_empty() {
                    i.display_name.as_str()
                } else if !i.tmux_session.is_empty() {
                    i.tmux_session.as_str()
                } else {
                    "session"
                };
                let base = base
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(80)
                    .collect::<String>();
                let base = base.trim();
                let base = if base.is_empty() { "session" } else { base };
                let mut name = base.to_owned();
                let mut suffix = 2;
                while c.sessions.values().any(|s| s.name == name) {
                    name = format!("{base}-{suffix}");
                    suffix += 1;
                }
                c.sessions.insert(
                    id.clone(),
                    Session {
                        id,
                        name,
                        provider: "codex".into(),
                        account: i.account.clone(),
                        conversation_id: i.thread_id.clone(),
                        revision: 1,
                        attachment,
                        directory: None,
                        previous_conversation_ids: vec![],
                    },
                );
            }
        }
        // A missing live binding means detached/stopped/unknown, never permission
        // to restart it on another account. Preserve conversation and owner.
        for s in c.sessions.values_mut() {
            // The same process now verifiably runs another conversation (native
            // thread switch in one terminal): release this entry to stopped so the
            // process is attached to exactly one catalog entry, never duplicated.
            if s.provider == "codex"
                && let Some(a) = &s.attachment
                && instances.iter().any(|i| {
                    !i.inactive
                        && !i.thread_id.is_empty()
                        && i.thread_id != s.conversation_id
                        && i.pid == a.pid
                        && i.start_time == a.process_start
                })
            {
                s.attachment = None;
                s.revision += 1;
                continue;
            }
            if s.attachment.is_some()
                && s.attachment.as_ref().is_some_and(|a| {
                    crate::external::attachment_definitely_gone(a.pid, a.process_start)
                })
                && !instances
                    .iter()
                    .any(|i| !i.inactive && i.thread_id == s.conversation_id)
            {
                s.attachment = None;
                s.revision += 1;
            }
        }
        Ok(c.sessions.values().cloned().collect())
    })
}

pub fn resolve(paths: &Paths, id: &str, generation: &str) -> Result<Session> {
    transaction(paths, |c| {
        if let Some(target) = c.aliases.get(id)
            && !c.sessions.contains_key(id)
        {
            anyhow::bail!(
                "SESSION_MERGED: this entry continues as session {target}; refresh the session list"
            );
        }
        let s = c.sessions.get(id).context("SESSION_NOT_FOUND")?;
        ensure!(s.generation() == generation, "STALE_TARGET");
        Ok(s.clone())
    })
}

pub(crate) fn invalidate(paths: &Paths, id: &str, generation: &str) -> Result<Session> {
    transaction(paths, |c| {
        let s = c.sessions.get_mut(id).context("SESSION_NOT_FOUND")?;
        ensure!(s.generation() == generation, "STALE_TARGET");
        s.attachment = None;
        s.revision += 1;
        Ok(s.clone())
    })
}
pub fn rename(paths: &Paths, id: &str, generation: &str, name: &str) -> Result<Session> {
    validate_name(name)?;
    transaction(paths, |c| {
        let current = c.sessions.get(id).context("SESSION_NOT_FOUND")?;
        ensure!(current.generation() == generation, "STALE_TARGET");
        if current.name == name {
            return Ok(current.clone());
        }
        ensure!(
            !c.sessions.values().any(|s| s.id != id && s.name == name)
                && !c.reservations.contains_key(name),
            "SESSION_NAME_EXISTS"
        );
        let s = c.sessions.get_mut(id).context("SESSION_NOT_FOUND")?;
        ensure!(s.generation() == generation, "STALE_TARGET");
        if s.name != name {
            s.name = name.into();
            s.revision += 1;
        }
        Ok(s.clone())
    })
}

/// A verified presentation move changes only attachment metadata, preserving a
/// concurrent rename. Another account/process can never be overwritten here.
pub(crate) fn move_attachment(
    paths: &Paths,
    before: &Session,
    attachment: Option<Attachment>,
) -> Result<Session> {
    transaction(paths, |c| {
        let current = c
            .sessions
            .get_mut(&before.id)
            .context("SESSION_NOT_FOUND")?;
        ensure!(
            current.provider == before.provider
                && current.account == before.account
                && current.conversation_id == before.conversation_id,
            "STALE_TARGET"
        );
        ensure!(
            current.attachment.is_none()
                || current.attachment == before.attachment
                || current.attachment == attachment,
            "STALE_TARGET"
        );
        if current.attachment != attachment {
            current.attachment = attachment;
            current.revision += 1;
        }
        Ok(current.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn seed(paths: &Paths) -> Session {
        let s = Session {
            id: uuid::Uuid::new_v4().to_string(),
            name: "first".into(),
            provider: "codex".into(),
            account: "selected".into(),
            conversation_id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            attachment: Some(Attachment {
                host: "herdr".into(),
                target: "herdr/test/w1:p1".into(),
                pid: 42,
                process_start: 99,
            }),
            directory: None,
            previous_conversation_ids: vec![],
        };
        transaction(paths, |c| {
            c.sessions.insert(s.id.clone(), s.clone());
            Ok(())
        })
        .unwrap();
        s
    }
    #[test]
    fn names_are_thirty_characters_but_legacy_identity_is_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let s = seed(&p);
        transaction(&p, |c| {
            c.sessions.get_mut(&s.id).unwrap().name = "legacy-long-session-name-over-limit".into();
            Ok(())
        })
        .unwrap();
        let legacy = list(&p).unwrap().remove(0);
        assert_eq!(legacy.name, "legacy-long-session-name-over-limit");
        assert_eq!(
            resolve(&p, &legacy.id, &legacy.generation())
                .unwrap()
                .conversation_id,
            s.conversation_id
        );
        assert!(rename(&p, &s.id, &legacy.generation(), &"x".repeat(31)).is_err());
        let renamed = rename(&p, &s.id, &legacy.generation(), "界".repeat(30).as_str()).unwrap();
        assert_eq!(renamed.id, s.id);
        for conversation in ["a", "b"] {
            let assigned = record_launch(
                &p,
                "selected",
                conversation,
                "long-provider-session-label-that-exceeds-thirty",
                None,
            )
            .unwrap();
            assert_eq!(assigned.name.chars().count(), 30);
        }
        assert_eq!(list(&p).unwrap().len(), 3);
    }
    #[test]
    fn rename_is_atomic_unique_and_rejects_old_generations() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let s = seed(&p);
        let old = s.generation();
        let renamed = rename(&p, &s.id, &old, "renamed").unwrap();
        assert_eq!(renamed.id, s.id);
        assert_eq!(renamed.conversation_id, s.conversation_id);
        assert!(resolve(&p, &s.id, &old).is_err());
        assert!(rename(&p, &s.id, &old, "stale").is_err());
        let second = seed(&p);
        assert!(rename(&p, &second.id, &second.generation(), "renamed").is_err());
        assert_eq!(list(&p).unwrap().len(), 2);
    }
    #[test]
    fn stable_identity_follows_account_and_runtime_replacements() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(t.path().join("data"))).unwrap();
        let before = seed(&p);
        let i:crate::external::Instance=serde_json::from_value(serde_json::json!({"pane":"herdr/test/w2:p3","tmux_session":"provider-label","pid":52,"start_time":999,"cwd":"/tmp","thread_id":before.conversation_id,"rollout":"/tmp/fixture","home":"/tmp/fixture-home","binary":"codex","resume_options":[],"account":"destination"})).unwrap();
        let after = reconcile(&p, &[i]).unwrap().remove(0);
        assert_eq!(before.id, after.id);
        assert_eq!(before.name, after.name);
        assert_eq!(before.conversation_id, after.conversation_id);
        assert_eq!(after.account, "destination");
        assert!(resolve(&p, &before.id, &before.generation()).is_err());
        assert_eq!(after.attachment.unwrap().pid, 52);
    }
    #[test]
    fn concurrent_renames_cannot_claim_one_name() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(t.path().join("data"))).unwrap();
        let first = seed(&p);
        let first = rename(&p, &first.id, &first.generation(), "one").unwrap();
        let second = seed(&p);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = [first, second]
            .into_iter()
            .map(|s| {
                let p = p.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    rename(&p, &s.id, &s.generation(), "shared").is_ok()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .filter(|w| w.thread().id() != std::thread::current().id())
                .map(|w| usize::from(w.join().unwrap()))
                .sum::<usize>(),
            1
        );
        let names = list(&p)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), 2);
    }
    #[test]
    fn partial_discovery_never_replaces_a_still_live_attachment() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let stat = fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
        let start: u64 = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let s = record_launch(
            &p,
            "selected",
            "11111111-1111-4111-8111-111111111111",
            "Live",
            Some(Attachment {
                host: "herdr".into(),
                target: "herdr/other/w1:p1".into(),
                pid: std::process::id(),
                process_start: start,
            }),
        )
        .unwrap();
        let saved: crate::external::Instance = serde_json::from_value(serde_json::json!({"inactive":true,"account":"other","pane":"","tmux_session":"","pid":0,"cwd":"/tmp","thread_id":s.conversation_id,"rollout":"/tmp/fixture","home":"/tmp/home","binary":"codex","resume_options":[]})).unwrap();
        for items in [vec![], vec![saved]] {
            let after = reconcile(&p, &items).unwrap().remove(0);
            assert_eq!(after.generation(), s.generation());
            assert_eq!(after.account, "selected");
            assert_eq!(after.attachment, s.attachment);
        }
        transaction(&p, |c| {
            c.sessions
                .get_mut(&s.id)
                .unwrap()
                .attachment
                .as_mut()
                .unwrap()
                .process_start += 1;
            Ok(())
        })
        .unwrap();
        assert!(
            reconcile(&p, &[]).unwrap()[0].attachment.is_none(),
            "PID reuse remains a positive detachment"
        );
    }
    #[test]
    fn disappeared_attachment_retains_owner_and_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let s = seed(&p);
        let result = reconcile(&p, &[]).unwrap();
        assert_eq!(result[0].account, "selected");
        assert_eq!(result[0].conversation_id, s.conversation_id);
        assert_eq!(result[0].id, s.id);
        assert!(result[0].attachment.is_none());
        assert!(resolve(&p, &s.id, &s.generation()).is_err());
    }
}

pub fn check_name(paths: &Paths, name: &str, request: &str) -> Result<()> {
    validate_name(name)?;
    transaction(paths, |c| {
        ensure!(
            !c.sessions.values().any(|s| s.name == name)
                && c.reservations.get(name).is_none_or(|r| r == request),
            "SESSION_NAME_EXISTS"
        );
        Ok(())
    })
}
pub fn reserve_name(paths: &Paths, name: &str, request: &str) -> Result<()> {
    validate_name(name)?;
    transaction(paths, |c| {
        ensure!(
            !c.sessions.values().any(|s| s.name == name)
                && c.reservations.get(name).is_none_or(|r| r == request),
            "SESSION_NAME_EXISTS"
        );
        c.reservations.insert(name.into(), request.into());
        Ok(())
    })
}
#[allow(clippy::too_many_arguments)]
pub fn complete_reserved(
    paths: &Paths,
    request: &str,
    name: &str,
    provider: ProviderId,
    account: &str,
    conversation: &str,
    directory: Option<&str>,
    attachment: Option<Attachment>,
) -> Result<Session> {
    transaction(paths, |c| {
        ensure!(
            c.reservations.get(name).is_some_and(|r| r == request),
            "NAME_RESERVATION_LOST"
        );
        ensure!(
            !c.sessions
                .values()
                .any(|s| s.name == name || s.conversation_id == conversation),
            "SESSION_NAME_EXISTS"
        );
        let s = Session {
            id: request.into(),
            name: name.into(),
            provider: provider.as_str().into(),
            account: account.into(),
            conversation_id: conversation.into(),
            revision: 1,
            attachment,
            directory: directory.map(str::to_owned),
            previous_conversation_ids: vec![],
        };
        c.reservations.remove(name);
        c.sessions.insert(s.id.clone(), s.clone());
        Ok(s)
    })
}

pub fn release_reservation(paths: &Paths, request: &str, name: &str) -> Result<()> {
    transaction(paths, |c| {
        if c.reservations.get(name).is_some_and(|r| r == request) {
            c.reservations.remove(name);
        }
        Ok(())
    })
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    #[test]
    fn reserved_names_are_unique_and_convert_without_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("root"))).unwrap();
        reserve_name(&paths, "chosen", "request-a").unwrap();
        assert!(reserve_name(&paths, "chosen", "request-b").is_err());
        let other = record_launch(&paths, "account", "conversation-b", "chosen", None).unwrap();
        assert_eq!(other.name, "chosen-2");
        assert!(rename(&paths, &other.id, &other.generation(), "chosen").is_err());
        let s = complete_reserved(
            &paths,
            "request-a",
            "chosen",
            ProviderId::Codex,
            "account",
            "conversation-a",
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.id, "request-a");
        assert_eq!(s.name, "chosen");
        assert_eq!(s.account, "account");
        assert!(check_name(&paths, "chosen", "request-a").is_err());
    }
}

#[cfg(test)]
mod space_move_tests {
    use super::*;
    #[test]
    fn verified_move_preserves_rename_is_idempotent_and_rejects_account_replacement() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let old = Attachment {
            host: "herdr".into(),
            target: "herdr/fixture/w1:p1".into(),
            pid: 11,
            process_start: 22,
        };
        let before = record_launch(
            &p,
            "example",
            &uuid::Uuid::new_v4().to_string(),
            "original",
            Some(old.clone()),
        )
        .unwrap();
        rename(&p, &before.id, &before.generation(), "renamed").unwrap();
        let new = Attachment {
            target: "herdr/fixture/w2:p2".into(),
            ..old
        };
        let moved = move_attachment(&p, &before, Some(new.clone())).unwrap();
        assert_eq!(moved.name, "renamed");
        assert_eq!(moved.conversation_id, before.conversation_id);
        assert_eq!(
            move_attachment(&p, &before, Some(new.clone()))
                .unwrap()
                .generation(),
            moved.generation()
        );
        record_launch(
            &p,
            "other",
            &before.conversation_id,
            "other",
            Some(new.clone()),
        )
        .unwrap();
        assert!(
            move_attachment(&p, &before, Some(new))
                .unwrap_err()
                .to_string()
                .contains("STALE_TARGET")
        );
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    fn paths() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(t.path().join("root"))).unwrap();
        (t, p)
    }
    fn found(account: &str, id: &str, label: &str) -> Discovered {
        Discovered {
            account: account.into(),
            conversation_id: id.into(),
            label: label.into(),
            directory: Some("/work".into()),
        }
    }
    #[test]
    fn continuation_inherits_the_entry_merges_the_duplicate_and_never_reappears() {
        let (_t, p) = paths();
        let claude = ProviderId::Claude;
        let all = register_discovered(
            &p,
            claude,
            &[found("main", "a", "work"), found("main", "b", "work")],
        )
        .unwrap();
        assert_eq!(all.len(), 2);
        let old = all.iter().find(|s| s.conversation_id == "a").unwrap();
        let old = rename(&p, &old.id, &old.generation(), "sessionstuffs").unwrap();
        let dup = all
            .iter()
            .find(|s| s.conversation_id == "b")
            .unwrap()
            .clone();
        // Cross-account evidence is never applied.
        let wrong = Continuation {
            account: "other".into(),
            from: "a".into(),
            to: "b".into(),
        };
        assert!(link_continuations(&p, claude, &[wrong]).unwrap().is_empty());
        assert_eq!(list(&p).unwrap().len(), 2);
        let link = Continuation {
            account: "main".into(),
            from: "a".into(),
            to: "b".into(),
        };
        let merged = link_continuations(&p, claude, &[link.clone(), link.clone()]).unwrap();
        assert_eq!(merged, vec![(dup.id.clone(), old.id.clone())]);
        let now = list(&p).unwrap();
        assert_eq!(now.len(), 1, "one lineage, one entry");
        assert_eq!(now[0].id, old.id);
        assert_eq!(now[0].name, "sessionstuffs");
        assert_eq!(now[0].account, "main");
        assert_eq!(now[0].conversation_id, "b");
        assert_eq!(now[0].previous_conversation_ids, ["a"]);
        let e = resolve(&p, &dup.id, &dup.generation())
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("SESSION_MERGED") && e.contains(&old.id),
            "{e}"
        );
        // Rediscovery of the continued transcript never recreates an entry.
        let again = register_discovered(
            &p,
            claude,
            &[found("main", "a", "x"), found("main", "b", "x")],
        )
        .unwrap();
        assert_eq!(again.len(), 1);
        // Chains follow to the newest continuation.
        register_discovered(&p, claude, &[found("main", "c", "x")]).unwrap();
        link_continuations(
            &p,
            claude,
            &[
                link,
                Continuation {
                    account: "main".into(),
                    from: "b".into(),
                    to: "c".into(),
                },
            ],
        )
        .unwrap();
        let now = list(&p).unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].conversation_id, "c");
        assert_eq!(now[0].previous_conversation_ids, ["a", "b"]);
    }
    #[test]
    fn codex_process_switching_threads_is_attached_to_exactly_one_entry() {
        let (_t, p) = paths();
        let pid = std::process::id();
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let start: u64 = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let attachment = Attachment {
            host: "tmux".into(),
            target: "%7".into(),
            pid,
            process_start: start,
        };
        let a = record_launch(&p, "acct", "thread-a", "first", Some(attachment.clone())).unwrap();
        let b = record_launch(&p, "acct", "thread-b", "second", None).unwrap();
        // The same TUI process now verifiably holds thread-b.
        let i: crate::external::Instance = serde_json::from_value(serde_json::json!({"pane":"%7","tmux_session":"t","pid":pid,"start_time":start,"cwd":"/tmp","thread_id":"thread-b","rollout":"/tmp/r","home":"/tmp/h","binary":"codex","resume_options":[],"account":"acct"})).unwrap();
        let all = reconcile(&p, &[i]).unwrap();
        assert_eq!(all.len(), 2, "no duplicate entry is created");
        let a2 = all.iter().find(|s| s.id == a.id).unwrap();
        let b2 = all.iter().find(|s| s.id == b.id).unwrap();
        assert!(a2.attachment.is_none(), "the entry it left is released");
        assert_eq!(a2.name, "first");
        assert_eq!(b2.attachment.as_ref(), Some(&attachment));
        assert_eq!(b2.name, "second");
        assert_eq!(
            all.iter()
                .filter(|s| s.attachment.as_ref().is_some_and(|x| x.pid == pid))
                .count(),
            1
        );
    }
}
