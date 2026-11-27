//! Stable backend identity, distinct from native conversations and attachments.
pub mod names;
pub mod placement;
pub mod scope;
use crate::paths::{Paths, atomic_write, lock_file, private_dir};
use crate::provider::ProviderId;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
pub use scope::{
    ParsedName, parse_name, record_space, record_space_best_effort, resolve_target, same_space,
    set_space, space_taken,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub host: String,
    pub target: String,
    pub pid: u32,
    pub process_start: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// HERDR Space (workspace) label the session belongs to (contract 1.48.0);
    /// None = unscoped legacy entry. Omitted when None. Not part of the
    /// generation hash (see [`Session::generation`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
}

impl Session {
    /// sha256 of the serialized entry WITHOUT `space`: recording, backfilling or
    /// healing a space never changes a generation by itself. Every explicit space
    /// change (set-space, move-space, resume into another Space, a host observation
    /// that moves a live session) also increments `revision`, so it does change
    /// the generation, like every other mutation.
    pub fn generation(&self) -> String {
        if self.space.is_none() {
            crate::generation::of(self).expect("serializable session")
        } else {
            let mut unscoped = self.clone();
            unscoped.space = None;
            crate::generation::of(&unscoped).expect("serializable session")
        }
    }
    /// `<space>:<name>`; None for an unscoped entry or a Space label containing `:`.
    pub fn canonical(&self) -> Option<String> {
        self.space
            .as_deref()
            .filter(|space| !space.contains(':'))
            .map(|space| format!("{space}:{}", self.name))
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Catalog {
    schema_version: u32,
    sessions: BTreeMap<String, Session>,
    #[serde(default)]
    reservations: BTreeMap<String, String>,
    /// Catalog ids merged into another entry by a verified continuation link.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    aliases: BTreeMap<String, String>,
    /// Transient: why a transaction changed an entry's space (sidecar `source`).
    #[serde(skip)]
    space_notes: BTreeMap<String, &'static str>,
}

/// Identity facts mirrored outside `catalog.json`: merged ids and continuation
/// lineage. Releases before contract revision 12 rewrite `catalog.json` with a
/// schema that has neither field, so a mixed-version writer silently dropped
/// them; this file is unknown to such releases and restores them.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Identity {
    schema_version: u32,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    /// Catalog id -> `previous_conversation_ids` (oldest first).
    #[serde(default)]
    lineage: BTreeMap<String, Vec<String>>,
}
impl Identity {
    fn of(c: &Catalog) -> Self {
        Self {
            schema_version: 1,
            aliases: c.aliases.clone(),
            lineage: c
                .sessions
                .values()
                .filter(|s| !s.previous_conversation_ids.is_empty())
                .map(|s| (s.id.clone(), s.previous_conversation_ids.clone()))
                .collect(),
        }
    }
}

/// What restoring the identity record changed in `catalog.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Healed {
    pub restored_aliases: Vec<String>,
    pub restored_lineage: Vec<String>,
    /// Entries whose `space` was restored from `spaces.json` (contract 1.48.0).
    pub restored_spaces: Vec<String>,
}

/// Restore merged ids and lineage the identity record holds but the catalog
/// lost (never removes or recreates an entry).
fn heal(c: &mut Catalog, identity: &Identity) -> Healed {
    let mut healed = Healed::default();
    for (alias, target) in &identity.aliases {
        if !c.sessions.contains_key(alias) && !c.aliases.contains_key(alias) {
            c.aliases.insert(alias.clone(), target.clone());
            healed.restored_aliases.push(alias.clone());
        }
    }
    // Merged targets that were merged again resolve to the current survivor.
    let snapshot = c.aliases.clone();
    for target in c.aliases.values_mut() {
        let mut hops = 0;
        while let Some(next) = snapshot.get(target.as_str())
            && hops <= snapshot.len()
        {
            *target = next.clone();
            hops += 1;
        }
    }
    for (id, previous) in &identity.lineage {
        let Some(s) = c.sessions.get_mut(id) else {
            continue;
        };
        let mut merged: Vec<String> = previous
            .iter()
            .filter(|p| **p != s.conversation_id)
            .cloned()
            .collect();
        for p in &s.previous_conversation_ids {
            if !merged.contains(p) {
                merged.push(p.clone());
            }
        }
        if merged != s.previous_conversation_ids {
            s.previous_conversation_ids = merged;
            s.revision += 1;
            healed.restored_lineage.push(id.clone());
        }
    }
    healed
}

fn transaction<T>(paths: &Paths, change: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
    healing_transaction(paths, |c, _| change(c))
}

/// What the three catalog files held after a transaction that had nothing to write: the
/// state a later transaction would reach again (healing and backfill already applied). It
/// is valid exactly while the files keep the recorded stamps, and it is only ever used under
/// `catalog.lock`, which every writer holds, so a write invalidates it (new inode, size or
/// timestamps). Mutations never take this path: they run the full locked transaction.
struct Settled {
    stamps: [Option<crate::file_cache::Stamp>; 3],
    catalog: Catalog,
    /// `catalog` as serialized (what a write would compare against).
    bytes: Vec<u8>,
    identity: Identity,
    spaces: scope::Sidecar,
}
static SETTLED: std::sync::Mutex<BTreeMap<std::path::PathBuf, std::sync::Arc<Settled>>> =
    std::sync::Mutex::new(BTreeMap::new());
/// Roots remembered at once (a supervisor serves one; test processes create many).
const SETTLED_ROOTS: usize = 8;

fn stamps_of(dir: &std::path::Path) -> Result<[Option<crate::file_cache::Stamp>; 3]> {
    let one = |name: &str| -> Result<Option<crate::file_cache::Stamp>> {
        match fs::metadata(dir.join(name)) {
            Ok(m) => Ok(Some(crate::file_cache::Stamp::of(&m))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    Ok([
        one("catalog.json")?,
        one("identity.json")?,
        one(scope::SIDECAR)?,
    ])
}
/// The settled state when the files still carry the stamps it was recorded with. Call with
/// `catalog.lock` held.
fn settled_hit(dir: &std::path::Path) -> Option<std::sync::Arc<Settled>> {
    let hit = SETTLED.lock().ok()?.get(dir)?.clone();
    let current = stamps_of(dir).ok()? == hit.stamps;
    #[cfg(test)]
    tests::settled_hit(current);
    current.then_some(hit)
}

/// Read-only access: the settled catalog without cloning it, else the full (healing)
/// transaction. The lock is taken and released like a transaction's, so a writer in flight
/// is waited for.
fn read_catalog<T>(paths: &Paths, read: impl FnOnce(&Catalog) -> Result<T>) -> Result<T> {
    let dir = paths.root.join(crate::layout::SESSION_CATALOG);
    private_dir(&dir)?;
    {
        let lock = lock_file(&dir.join("catalog.lock"))?;
        lock.lock_exclusive()?;
        if let Some(settled) = settled_hit(&dir) {
            drop(lock);
            return read(&settled.catalog);
        }
    }
    healing_transaction(paths, |c, _| read(c))
}

fn healing_transaction<T>(
    paths: &Paths,
    change: impl FnOnce(&mut Catalog, &Healed) -> Result<T>,
) -> Result<T> {
    let dir = paths.root.join(crate::layout::SESSION_CATALOG);
    private_dir(&dir)?;
    let lock = lock_file(&dir.join("catalog.lock"))?;
    lock.lock_exclusive()?;
    let file = dir.join("catalog.json");
    let identity_file = dir.join("identity.json");
    let spaces_file = dir.join(scope::SIDECAR);
    let started = stamps_of(&dir)?;
    let (mut catalog, identity, spaces, before, healed, backfill, cached) = if let Some(s) =
        settled_hit(&dir)
    {
        // Nothing to heal or backfill: the files are what the last quiet transaction left.
        (
            s.catalog.clone(),
            s.identity.clone(),
            s.spaces.clone(),
            s.bytes.clone(),
            Healed::default(),
            None,
            Some(s),
        )
    } else {
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
        let identity: Identity = match fs::read(&identity_file) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("UNREADABLE_IDENTITY_RECORD")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Identity::default(),
            Err(e) => return Err(e.into()),
        };
        let spaces = scope::read_sidecar(&spaces_file)?;
        let before = serde_json::to_vec(&catalog)?;
        let mut healed = heal(&mut catalog, &identity);
        healed.restored_spaces = scope::heal(&mut catalog, &spaces);
        let backfill = scope::backfill(paths, &mut catalog, &spaces);
        (catalog, identity, spaces, before, healed, backfill, None)
    };
    let result = change(&mut catalog, &healed)?;
    if let Some(s) = &cached
        && catalog == s.catalog
    {
        // A settled catalog the closure left alone: no healing, nothing to write.
        return Ok(result);
    }
    let after = serde_json::to_vec(&catalog)?;
    // The space record is written first too (same crash ordering as identity).
    let next = scope::next_sidecar(&mut catalog, &spaces, backfill);
    let wrote_spaces = next != spaces || !spaces_file.exists();
    if wrote_spaces {
        atomic_write(&spaces_file, &serde_json::to_vec(&next)?)?;
    }
    // The identity record is written first: a crash between the two writes
    // leaves a record the next transaction restores from, never a loss.
    let next = Identity::of(&catalog);
    let wrote_identity =
        next != identity && (identity_file.exists() || next != Identity::of(&Catalog::default()));
    if wrote_identity {
        atomic_write(&identity_file, &serde_json::to_vec(&next)?)?;
    }
    let wrote_catalog = before != after || !file.exists();
    if wrote_catalog {
        atomic_write(&file, &after)?;
    }
    // Nothing was written: remember this state for the next transaction (see `Settled`).
    if !wrote_catalog && !wrote_identity && !wrote_spaces {
        let settled = Settled {
            stamps: started,
            catalog,
            bytes: after,
            identity,
            spaces,
        };
        if settled.stamps.iter().flatten().all(|s| s.settled())
            && let Ok(mut map) = SETTLED.lock()
        {
            if map.len() >= SETTLED_ROOTS && !map.contains_key(&dir) {
                map.clear();
            }
            map.insert(dir, std::sync::Arc::new(settled));
        }
    }
    Ok(result)
}

/// The live entry `id`, or `SESSION_MERGED` naming its survivor.
fn live<'a>(c: &'a mut Catalog, id: &str) -> Result<&'a mut Session> {
    live_ref(c, id)?;
    c.sessions.get_mut(id).context("SESSION_NOT_FOUND")
}
fn live_ref<'a>(c: &'a Catalog, id: &str) -> Result<&'a Session> {
    if !c.sessions.contains_key(id)
        && let Some(target) = c.aliases.get(id)
    {
        anyhow::bail!(
            "SESSION_MERGED: this entry continues as session {target}; refresh the session list"
        );
    }
    c.sessions.get(id).context("SESSION_NOT_FOUND")
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
    read_catalog(paths, |c| Ok(c.sessions.values().cloned().collect()))
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
    record_provider_launch_as(
        paths,
        provider,
        account,
        conversation,
        label,
        directory,
        attachment,
        None,
    )
}
/// The catalog id of a provider conversation, when it has an entry.
pub fn id_for_conversation(
    paths: &Paths,
    provider: ProviderId,
    conversation: &str,
) -> Option<String> {
    read_catalog(paths, |c| {
        Ok(c.sessions
            .values()
            .find(|s| s.provider == provider.as_str() && s.conversation_id == conversation)
            .map(|s| s.id.clone()))
    })
    .ok()
    .flatten()
}
/// [`record_provider_launch`] with the id a new entry gets (contract 1.62.0: a Codex
/// thread is told its session id before the entry exists). An existing entry keeps its id.
#[allow(clippy::too_many_arguments)]
pub fn record_provider_launch_as(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
    conversation: &str,
    label: &str,
    directory: Option<&str>,
    attachment: Option<Attachment>,
    preferred_id: Option<&str>,
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
        let name = unique_name(label, |name| scope::taken_anywhere(c, name));
        let s = Session {
            id: preferred_id
                .filter(|id| !c.sessions.contains_key(*id))
                .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned),
            name,
            provider: provider.as_str().into(),
            account: account.into(),
            conversation_id: conversation.into(),
            revision: 1,
            attachment,
            directory: directory.map(str::to_owned),
            previous_conversation_ids: vec![],
            space: None,
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
            let name = unique_name(&d.label, |name| scope::taken_anywhere(c, name));
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
                space: None,
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

/// The conversations of one entry, oldest first (previous ids, then current).
fn chain(s: &Session) -> Vec<String> {
    let mut all = s.previous_conversation_ids.clone();
    all.push(s.conversation_id.clone());
    all
}

/// Merge `dup` into `survivor`. The survivor keeps its id, name, account and
/// history; `dup` becomes a persisted alias (`SESSION_MERGED`). When `dup`'s
/// current conversation already belongs to the survivor's lineage, `dup` was a
/// re-registration of an older conversation and the survivor's conversation is
/// kept; otherwise the survivor moves on to `dup`'s (newer) conversation.
fn merge(c: &mut Catalog, dup: &str, survivor: &str, merged: &mut Vec<(String, String)>) {
    let Some(d) = c.sessions.remove(dup) else {
        return;
    };
    for target in c.aliases.values_mut() {
        if *target == d.id {
            *target = survivor.to_owned();
        }
    }
    c.aliases.insert(d.id.clone(), survivor.to_owned());
    merged.push((d.id.clone(), survivor.to_owned()));
    let Some(s) = c.sessions.get_mut(survivor) else {
        return;
    };
    let mut lineage = chain(s);
    let theirs = chain(&d);
    let follows = if let Some(at) = lineage.iter().position(|x| *x == d.conversation_id) {
        // Re-registration of a conversation already in the lineage: only its
        // unknown earlier conversations are added, before it.
        let missing: Vec<String> = theirs
            .iter()
            .filter(|x| !lineage.contains(x))
            .cloned()
            .collect();
        lineage.splice(at..at, missing);
        false
    } else {
        for x in theirs {
            if !lineage.contains(&x) {
                lineage.push(x);
            }
        }
        true
    };
    s.conversation_id = lineage.pop().expect("non-empty lineage");
    s.previous_conversation_ids = lineage;
    // Only the process of the conversation the survivor now follows moves with
    // it; an attachment of an older, re-registered conversation never does.
    if follows && d.attachment.is_some() {
        s.attachment = d.attachment;
    }
    if s.directory.is_none() {
        s.directory = d.directory;
    }
    s.revision += 1;
}

/// Link verified continuations without ever deleting and recreating identity.
///
/// The survivor of a lineage is the entry that already records it (`from` in
/// its `previous_conversation_ids`); otherwise the pre-existing entry of the
/// earliest conversation (`from`), which keeps its stable id, name and account
/// and moves to `to`. Any other entry of a conversation in the lineage is merged
/// into the survivor and kept as an alias, so one lineage has one entry. With no
/// entry for `from`, the entry of `to` records `from` as previous. Cross-account
/// or cross-provider links are never applied. Idempotent and independent of
/// link order. Returns the merged (alias, survivor) ids.
pub fn link_continuations(
    paths: &Paths,
    provider: ProviderId,
    links: &[Continuation],
) -> Result<Vec<(String, String)>> {
    transaction(paths, |c| {
        let mut merged = vec![];
        let candidates = |c: &Catalog, account: &str, pick: &dyn Fn(&Session) -> bool| {
            c.sessions
                .values()
                .filter(|s| s.provider == provider.as_str() && s.account == account && pick(s))
                .map(|s| s.id.clone())
                .collect::<Vec<_>>()
        };
        // Each pass either merges an entry or advances one; bounded.
        for _ in 0..=(links.len() + c.sessions.len()) {
            let mut changed = false;
            for link in links {
                if link.from == link.to {
                    continue;
                }
                // A link whose conversations belong to another account is never applied.
                let foreign = c.sessions.values().any(|s| {
                    s.provider == provider.as_str()
                        && s.account != link.account
                        && (s.conversation_id == link.from || s.conversation_id == link.to)
                });
                if foreign {
                    continue;
                }
                let recorded = candidates(c, &link.account, &|s| {
                    s.previous_conversation_ids.contains(&link.from)
                });
                let survivor = match recorded.as_slice() {
                    [one] => Some(one.clone()),
                    [] => candidates(c, &link.account, &|s| s.conversation_id == link.from)
                        .into_iter()
                        .next(),
                    // Two entries already claim the same lineage: never guess.
                    _ => continue,
                };
                let lineage = |s: &Session| {
                    s.conversation_id == link.from
                        || s.conversation_id == link.to
                        || s.previous_conversation_ids.contains(&link.to)
                };
                match survivor {
                    Some(survivor) => {
                        for dup in candidates(c, &link.account, &lineage) {
                            if dup != survivor {
                                merge(c, &dup, &survivor, &mut merged);
                                changed = true;
                            }
                        }
                        let s = c.sessions.get_mut(&survivor).context("SESSION_NOT_FOUND")?;
                        if s.conversation_id == link.from {
                            s.previous_conversation_ids.push(link.from.clone());
                            s.conversation_id = link.to.clone();
                            s.revision += 1;
                            changed = true;
                        }
                    }
                    None => {
                        let owners = candidates(c, &link.account, &|s| {
                            s.conversation_id == link.to
                                || s.previous_conversation_ids.contains(&link.to)
                        });
                        let [owner] = owners.as_slice() else {
                            continue;
                        };
                        let s = c.sessions.get_mut(owner).context("SESSION_NOT_FOUND")?;
                        let at = s
                            .previous_conversation_ids
                            .iter()
                            .position(|x| *x == link.to)
                            .unwrap_or(s.previous_conversation_ids.len());
                        s.previous_conversation_ids.insert(at, link.from.clone());
                        s.revision += 1;
                        changed = true;
                    }
                }
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
                .find(|s| {
                    crate::provider::ops::supervised(&s.provider)
                        && s.conversation_id == i.thread_id
                })
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
                while scope::taken_anywhere(c, &name) {
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
                        space: None,
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
            if crate::provider::ops::supervised(&s.provider)
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
    read_catalog(paths, |c| {
        let s = live_ref(c, id)?;
        ensure!(s.generation() == generation, "STALE_TARGET");
        Ok(s.clone())
    })
}

/// The current entry `id` without a generation check (`SESSION_MERGED` for a
/// merged id).
pub fn find(paths: &Paths, id: &str) -> Result<Session> {
    read_catalog(paths, |c| Ok(live_ref(c, id)?.clone()))
}

/// Explicit, idempotent identity repair: restore merged ids and continuation
/// lineage recorded in the identity file but lost from `catalog.json` (for
/// example by an older release rewriting it). It never removes, recreates or
/// renames an entry and never invents an id.
pub fn repair_identity(paths: &Paths) -> Result<Healed> {
    healing_transaction(paths, |_, healed| Ok(healed.clone()))
}

/// Merged ids (alias -> survivor) currently recorded.
pub fn aliases(paths: &Paths) -> Result<BTreeMap<String, String>> {
    read_catalog(paths, |c| Ok(c.aliases.clone()))
}

pub(crate) fn invalidate(paths: &Paths, id: &str, generation: &str) -> Result<Session> {
    transaction(paths, |c| {
        let s = live(c, id)?;
        ensure!(s.generation() == generation, "STALE_TARGET");
        s.attachment = None;
        s.revision += 1;
        Ok(s.clone())
    })
}
/// Permanently remove a deleted conversation's entry and every merged id
/// (alias) that resolves to it; the identity record drops them in the same
/// transaction. Its identity (provider, account, conversation lineage) must be
/// unchanged since `before` was read; attachment/directory observations may differ.
pub(crate) fn remove(paths: &Paths, before: &Session) -> Result<Session> {
    transaction(paths, |c| {
        let current = live(c, &before.id)?;
        ensure!(
            current.provider == before.provider
                && current.account == before.account
                && current.conversation_id == before.conversation_id
                && current.previous_conversation_ids == before.previous_conversation_ids,
            "STALE_TARGET"
        );
        let removed = c.sessions.remove(&before.id).context("SESSION_NOT_FOUND")?;
        c.aliases.retain(|_, target| *target != removed.id);
        Ok(removed)
    })
}
/// Point every session of `provider:old` at `new` (account backend rename).
/// Idempotent; returns how many entries changed. Each changed entry gets a new
/// revision, so its generation changes and clients refresh.
pub fn reassign_account(paths: &Paths, provider: &str, old: &str, new: &str) -> Result<usize> {
    transaction(paths, |c| {
        let mut changed = 0;
        for s in c
            .sessions
            .values_mut()
            .filter(|s| s.provider == provider && s.account == old)
        {
            s.account = new.into();
            s.revision += 1;
            changed += 1;
        }
        Ok(changed)
    })
}
/// Move a Claude entry's account after its conversation files were moved
/// (contract 1.34.0). Idempotent: an entry already on `to` is returned
/// unchanged. The process attachment is cleared (the old process is gone) and
/// the revision changes, so the generation changes and clients refresh.
pub(crate) fn switch_account(
    paths: &Paths,
    id: &str,
    from: &str,
    to: &str,
    conversation: &str,
) -> Result<Session> {
    transaction(paths, |c| {
        let s = live(c, id)?;
        ensure!(
            crate::provider::ops::holds(&s.provider, |f| f.native_account_switch)
                && s.conversation_id == conversation,
            "STALE_TARGET"
        );
        if s.account == to {
            return Ok(s.clone());
        }
        ensure!(s.account == from, "STALE_TARGET");
        s.account = to.into();
        s.attachment = None;
        s.revision += 1;
        Ok(s.clone())
    })
}
/// Root re-key (contract 1.54.0): point a session's stored directory at its
/// new spelling. Idempotent (an entry already on `to` is returned unchanged);
/// refuses when the identity or the stored directory changed since `from` was
/// read. Returns (revision before, entry after).
pub(crate) fn rekey_directory(
    paths: &Paths,
    id: &str,
    conversation: &str,
    from: Option<&str>,
    to: &str,
) -> Result<(u64, Session)> {
    transaction(paths, |c| {
        let s = live(c, id)?;
        ensure!(s.conversation_id == conversation, "STALE_TARGET");
        if s.directory.as_deref() == Some(to) {
            return Ok((s.revision, s.clone()));
        }
        ensure!(s.directory.as_deref() == from, "STALE_TARGET");
        let before = s.revision;
        s.directory = Some(to.to_owned());
        s.revision += 1;
        Ok((before, s.clone()))
    })
}
/// Undo [`rekey_directory`] exactly: the old directory and, when nothing else
/// changed the entry since, the old revision (so the generation is the original
/// one again). Returns whether the entry changed.
pub(crate) fn restore_directory(
    paths: &Paths,
    id: &str,
    conversation: &str,
    to: &str,
    from: Option<&str>,
    revision_before: u64,
    revision_after: u64,
) -> Result<bool> {
    transaction(paths, |c| {
        let Some(s) = c.sessions.get_mut(id) else {
            return Ok(false);
        };
        if s.conversation_id != conversation || s.directory.as_deref() != Some(to) {
            return Ok(false);
        }
        s.directory = from.map(str::to_owned);
        if s.revision == revision_after {
            s.revision = revision_before;
        } else {
            s.revision += 1;
        }
        Ok(true)
    })
}

/// `api rename`: a bare name, or `<space>:<name>` naming the session's own Space
/// (rename never moves: `SPACE_MISMATCH`). Unique within the session's Space,
/// case-insensitive; a byte-equal name is a no-op with the same generation.
pub fn rename(paths: &Paths, id: &str, generation: &str, name: &str) -> Result<Session> {
    let parsed = parse_name(name)?;
    transaction(paths, |c| {
        let current = live(c, id)?;
        ensure!(current.generation() == generation, "STALE_TARGET");
        let space = current.space.clone();
        scope::ensure_agrees(
            parsed,
            space.as_deref(),
            if space.is_none() {
                "the session is unscoped; use `api session set-space`"
            } else {
                "rename never moves; use `api move-space`"
            },
        )?;
        if current.name == parsed.name {
            return Ok(current.clone());
        }
        ensure!(
            !scope::taken(c, space.as_deref(), parsed.name, Some(id), None),
            "SESSION_NAME_EXISTS: \"{}\" is already used in this Space",
            parsed.name
        );
        let s = c.sessions.get_mut(id).context("SESSION_NOT_FOUND")?;
        s.name = parsed.name.into();
        s.revision += 1;
        Ok(s.clone())
    })
}

/// Rename (bare `name`) and place the entry in `space` in one generation-checked
/// step: a resume into a Space. Unique in the destination Space.
pub(crate) fn rename_in_space(
    paths: &Paths,
    id: &str,
    generation: &str,
    name: &str,
    space: Option<&str>,
    source: &'static str,
) -> Result<Session> {
    rename_checked(paths, id, generation, name, space, None, source)
}

/// Like `rename_in_space`; `unique_in` (when given) is the Space the name must
/// be unique in instead of the entry's resulting Space (a resume that will land
/// in a destination Space but records it only on a verified start).
pub(crate) fn rename_checked(
    paths: &Paths,
    id: &str,
    generation: &str,
    name: &str,
    space: Option<&str>,
    unique_in: Option<&str>,
    source: &'static str,
) -> Result<Session> {
    transaction(paths, |c| {
        let current = live(c, id)?;
        ensure!(current.generation() == generation, "STALE_TARGET");
        // A kept legacy name (e.g. over 64 characters) is not re-validated.
        if current.name != name {
            crate::launch::validate_session_name(name)?;
            ensure!(!name.contains(':'), "INVALID_SESSION_NAME");
        }
        let space = space.map(str::to_owned).or(current.space.clone());
        if current.name == name && current.space == space {
            return Ok(current.clone());
        }
        let check = unique_in.or(space.as_deref());
        ensure!(
            !scope::taken(c, check, name, Some(id), None),
            "SESSION_NAME_EXISTS: \"{name}\" is already used in this Space"
        );
        let s = c.sessions.get_mut(id).context("SESSION_NOT_FOUND")?;
        if s.name != name {
            s.name = name.into();
            s.revision += 1;
        }
        scope::assign(s, &mut c.space_notes, space, source, false);
        Ok(s.clone())
    })
}

/// A verified presentation move changes only attachment metadata (and, with
/// `space`, the verified destination Space), preserving a concurrent rename.
/// Another account/process can never be overwritten here.
pub(crate) fn move_attachment(
    paths: &Paths,
    before: &Session,
    attachment: Option<Attachment>,
    space: Option<String>,
) -> Result<Session> {
    transaction(paths, |c| {
        let current = live(c, &before.id)?;
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
        let s = c
            .sessions
            .get_mut(&before.id)
            .context("SESSION_NOT_FOUND")?;
        if space.is_some() {
            scope::assign(s, &mut c.space_notes, space, "move_space", true);
        }
        Ok(s.clone())
    })
}

#[cfg(test)]
mod tests;

/// Admission check: `name` is free in `space` (unique per Space, case-insensitive)
/// or reserved by this request.
pub fn check_name(paths: &Paths, space: Option<&str>, name: &str, request: &str) -> Result<()> {
    validate_name(name)?;
    transaction(paths, |c| {
        ensure!(
            !scope::taken(c, space, name, None, Some(request)),
            "SESSION_NAME_EXISTS: \"{name}\" is already used in this Space"
        );
        Ok(())
    })
}
/// Contract 1.73.1: the requests whose reservations claim `name` in `space`.
pub(crate) fn reservation_holders(paths: &Paths, space: Option<&str>, name: &str) -> Vec<String> {
    let claims =
        |(k, r): (&String, &String)| scope::reservation_claims(k, space, name).then(|| r.clone());
    read_catalog(paths, |c| {
        Ok(c.reservations.iter().filter_map(claims).collect())
    })
    .unwrap_or_default()
}
pub fn reserve_name(paths: &Paths, space: Option<&str>, name: &str, request: &str) -> Result<()> {
    validate_name(name)?;
    transaction(paths, |c| {
        ensure!(
            !scope::taken(c, space, name, None, Some(request)),
            "SESSION_NAME_EXISTS: \"{name}\" is already used in this Space"
        );
        c.reservations.retain(|_, r| r != request);
        c.reservations
            .insert(scope::scope_key(space, name), request.into());
        Ok(())
    })
}
#[allow(clippy::too_many_arguments)]
pub fn complete_reserved(
    paths: &Paths,
    request: &str,
    space: Option<&str>,
    name: &str,
    provider: ProviderId,
    account: &str,
    conversation: &str,
    directory: Option<&str>,
    attachment: Option<Attachment>,
) -> Result<Session> {
    transaction(paths, |c| {
        ensure!(
            c.reservations.values().any(|r| r == request),
            "NAME_RESERVATION_LOST"
        );
        ensure!(
            !scope::taken(c, space, name, None, Some(request))
                && !c
                    .sessions
                    .values()
                    .any(|s| s.conversation_id == conversation),
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
            space: space.map(str::to_owned),
        };
        c.reservations.retain(|_, r| r != request);
        c.space_notes.insert(s.id.clone(), "launch");
        c.sessions.insert(s.id.clone(), s.clone());
        Ok(s)
    })
}

/// The first free name for `label`: control characters dropped, at most
/// `SESSION_NAME_MAX` characters, BASE, else BASE-2, BASE-3 ... while `taken`.
fn unique_name(label: &str, taken: impl Fn(&str) -> bool) -> String {
    let base: String = label
        .chars()
        .filter(|c| !c.is_control())
        .take(crate::model::SESSION_NAME_MAX)
        .collect();
    let base = base.trim();
    let base = if base.is_empty() { "session" } else { base };
    let mut name = base.to_owned();
    let mut suffix = 2;
    while taken(&name) {
        let tail = format!("-{suffix}");
        let prefix: String = base
            .chars()
            .take(crate::model::SESSION_NAME_MAX.saturating_sub(tail.len()))
            .collect();
        name = format!("{prefix}{tail}");
        suffix += 1;
    }
    name
}
/// First free session name in `space`: BASE, else BASE-2, BASE-3 ... (as unnamed
/// launches record).
pub fn free_name(paths: &Paths, space: Option<&str>, label: &str) -> Result<String> {
    transaction(paths, |c| {
        let name = unique_name(label, |name| scope::taken(c, space, name, None, None));
        Ok(name)
    })
}
/// Release whatever name `request` reserved (one request reserves one name).
pub fn release_reservation(paths: &Paths, request: &str) -> Result<()> {
    transaction(paths, |c| {
        c.reservations.retain(|_, r| r != request);
        Ok(())
    })
}
