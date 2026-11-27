//! Space-scoped session names (contract 1.48.0).
//!
//! A session's `space` is the HERDR Space (workspace) label it belongs to. Names
//! are unique per Space (case-insensitive), the canonical form is
//! `<space>:<name>`, and `space` is mirrored per id in `spaces.json`, a sidecar
//! releases before 1.48.0 never touch: a rollback that rewrites `catalog.json`
//! without `space` is healed from it, like `identity.json` heals aliases.
use super::*;
use serde_json::Value;

/// Sidecar file name, next to `catalog.json` and `identity.json`.
pub(super) const SIDECAR: &str = "spaces.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Sidecar {
    schema_version: u32,
    /// Set once the one-time backfill ran; absent means "not yet".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backfilled_at: Option<i64>,
    #[serde(default)]
    sessions: BTreeMap<String, Entry>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    space: Option<String>,
    set_at: i64,
    source: String,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub(super) fn read_sidecar(file: &std::path::Path) -> Result<Sidecar> {
    match fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("UNREADABLE_SPACE_RECORD"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Sidecar {
            schema_version: 1,
            ..Default::default()
        }),
        Err(e) => Err(e.into()),
    }
}

/// Restore `space` the sidecar holds but `catalog.json` lost (a release before
/// 1.48.0 rewrote it). Restoring is not a change of the session: no revision, so
/// the generation is the one it had before the rollback.
pub(super) fn heal(c: &mut Catalog, sidecar: &Sidecar) -> Vec<String> {
    let mut restored = vec![];
    for s in c.sessions.values_mut() {
        if s.space.is_none()
            && let Some(Entry {
                space: Some(space), ..
            }) = sidecar.sessions.get(&s.id)
        {
            s.space = Some(space.clone());
            restored.push(s.id.clone());
        }
    }
    restored
}

fn usable_label(label: &str) -> bool {
    crate::paths::validate_space_label(label).is_ok()
}

fn read_json(path: &std::path::Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// One-time backfill (pure file reads, no host call): placement evidence, else the
/// auto-start record's workspace label, else unscoped. Never renames, never checks
/// uniqueness and never changes a revision or generation. Returns the backfill time
/// when it ran.
pub(super) fn backfill(paths: &Paths, c: &mut Catalog, sidecar: &Sidecar) -> Option<i64> {
    if sidecar.backfilled_at.is_some() {
        return None;
    }
    let placements = read_json(
        &paths
            .root
            .join(crate::layout::SESSION_CATALOG)
            .join("placements.json"),
    );
    let auto = read_json(&paths.root.join(crate::layout::AUTO_RECOVER));
    for s in c.sessions.values_mut() {
        if s.space.is_some() || sidecar.sessions.contains_key(&s.id) {
            continue;
        }
        let placed = placements["sessions"][&s.id]["space"]["label"]
            .as_str()
            .filter(|l| usable_label(l));
        let recovered = auto["sessions"][&s.id]["workspace_label"]
            .as_str()
            .filter(|l| usable_label(l));
        let (space, source) = match (placed, recovered) {
            (Some(l), _) => (Some(l.to_owned()), "placement_evidence"),
            (None, Some(l)) => (Some(l.to_owned()), "auto_recover"),
            (None, None) => (None, "backfill_null"),
        };
        s.space = space;
        c.space_notes.insert(s.id.clone(), source);
    }
    Some(now())
}

/// The sidecar after a transaction: one entry per catalog entry (null = unscoped);
/// entries of ids that left the catalog are dropped (merged ids keep theirs).
pub(super) fn next_sidecar(c: &mut Catalog, before: &Sidecar, backfill: Option<i64>) -> Sidecar {
    let mut next = before.clone();
    next.schema_version = 1;
    if backfill.is_some() {
        next.backfilled_at = backfill;
    }
    let notes = std::mem::take(&mut c.space_notes);
    let at = now();
    for s in c.sessions.values() {
        if next.sessions.get(&s.id).map(|e| &e.space) != Some(&s.space) {
            let source = notes.get(&s.id).copied().unwrap_or(if s.space.is_some() {
                "launch"
            } else {
                "unscoped"
            });
            next.sessions.insert(
                s.id.clone(),
                Entry {
                    space: s.space.clone(),
                    set_at: at,
                    source: source.into(),
                },
            );
        }
    }
    next.sessions
        .retain(|id, _| c.sessions.contains_key(id) || c.aliases.contains_key(id));
    next
}

/// Case-insensitive (Unicode lowercase) comparison of names and Space labels.
pub(crate) fn fold(s: &str) -> String {
    s.to_lowercase()
}
/// Same scope: both unscoped, or both in Spaces whose labels fold equal.
pub fn same_space(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => fold(a) == fold(b),
        _ => false,
    }
}
/// Reservation key: `fold(space)/fold(name)` (`/` is invalid in both parts).
pub(super) fn scope_key(space: Option<&str>, name: &str) -> String {
    format!("{}/{}", space.map(fold).unwrap_or_default(), fold(name))
}
/// Whether a reservation key claims `name` in `space` (keys written before 1.48.0
/// are the bare name, exact).
pub(super) fn reservation_claims(key: &str, space: Option<&str>, name: &str) -> bool {
    if key.contains('/') {
        key == scope_key(space, name)
    } else {
        key == name
    }
}
/// `name` is taken in `space` by another entry or by another request's reservation.
pub(super) fn taken(
    c: &Catalog,
    space: Option<&str>,
    name: &str,
    except: Option<&str>,
    request: Option<&str>,
) -> bool {
    let folded = fold(name);
    c.sessions.values().any(|s| {
        Some(s.id.as_str()) != except
            && same_space(s.space.as_deref(), space)
            && fold(&s.name) == folded
    }) || c
        .reservations
        .iter()
        .any(|(key, r)| Some(r.as_str()) != request && reservation_claims(key, space, name))
}

/// Whether any entry (in any Space) or reservation uses `name`: automatic
/// names for a row whose Space is not yet known avoid every Space it could land in.
pub(super) fn taken_anywhere(c: &Catalog, name: &str) -> bool {
    let folded = fold(name);
    c.sessions.values().any(|s| fold(&s.name) == folded) || taken(c, None, name, None, None)
}

/// Whether `name` is used by another entry (or reserved) in `space`.
pub fn space_taken(paths: &Paths, space: Option<&str>, name: &str, except: &str) -> Result<bool> {
    super::read_catalog(paths, |c| Ok(taken(c, space, name, Some(except), None)))
}

/// A parsed `--name`/`name`: a bare name, or `<space>:<name>`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParsedName<'a> {
    pub space: Option<&'a str>,
    pub name: &'a str,
}

/// Bare: the session-name rules and no `:`; canonical: exactly one `:` with a valid
/// Space label before it and a valid bare name after it. Anything else is
/// `INVALID_SESSION_NAME`. Nothing is trimmed.
pub fn parse_name(input: &str) -> Result<ParsedName<'_>> {
    match input.matches(':').count() {
        0 => {
            crate::launch::validate_session_name(input)?;
            Ok(ParsedName {
                space: None,
                name: input,
            })
        }
        1 => {
            let (space, name) = input.split_once(':').expect("one separator");
            ensure!(
                !space.is_empty() && !name.is_empty() && usable_label(space),
                "INVALID_SESSION_NAME"
            );
            crate::launch::validate_session_name(name)?;
            Ok(ParsedName {
                space: Some(space),
                name,
            })
        }
        _ => anyhow::bail!("INVALID_SESSION_NAME"),
    }
}

/// `SPACE_MISMATCH` unless the space part of a canonical name agrees (case-insensitive)
/// with `space`. A bare name always agrees.
pub fn ensure_agrees(parsed: ParsedName<'_>, space: Option<&str>, hint: &str) -> Result<()> {
    if let Some(part) = parsed.space {
        ensure!(
            space.is_some_and(|s| fold(s) == fold(part)),
            "SPACE_MISMATCH: the name's space \"{part}\" differs from {}; {hint}",
            space.map_or_else(|| "an unscoped session".to_owned(), |s| format!("\"{s}\""))
        );
    }
    Ok(())
}

fn ambiguous(target: &str, rows: &[&Session]) -> anyhow::Error {
    let listed = rows
        .iter()
        .map(|s| s.canonical().unwrap_or_else(|| s.id.clone()))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::anyhow!(
        "AMBIGUOUS_SESSION: {} sessions match \"{target}\" ({listed}); use the canonical <space>:<name> or the session id",
        rows.len()
    )
}

/// UUID, then merged id (`SESSION_MERGED`), then `<space>:<name>`, then bare name.
/// An exact (case-sensitive) name match wins over a case-insensitive one; more
/// than one match is `AMBIGUOUS_SESSION`, none `SESSION_NOT_FOUND`.
pub(super) fn target_id(c: &Catalog, target: &str) -> Result<String> {
    if c.sessions.contains_key(target) {
        return Ok(target.to_owned());
    }
    if let Some(survivor) = c.aliases.get(target) {
        anyhow::bail!(
            "SESSION_MERGED: this entry continues as session {survivor}; refresh the session list"
        );
    }
    let (space, name) = match target.split_once(':') {
        Some((space, name)) if !space.is_empty() && !name.is_empty() && !name.contains(':') => {
            (Some(space), name)
        }
        _ => (None, target),
    };
    let candidates: Vec<&Session> = c
        .sessions
        .values()
        .filter(|s| space.is_none_or(|sp| s.space.as_deref().is_some_and(|x| fold(x) == fold(sp))))
        .collect();
    let exact: Vec<&Session> = candidates
        .iter()
        .copied()
        .filter(|s| s.name == name)
        .collect();
    match exact.as_slice() {
        [one] => return Ok(one.id.clone()),
        [] => {}
        many => return Err(ambiguous(target, many)),
    }
    let folded = fold(name);
    let loose: Vec<&Session> = candidates
        .into_iter()
        .filter(|s| fold(&s.name) == folded)
        .collect();
    match loose.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => anyhow::bail!("SESSION_NOT_FOUND"),
        many => Err(ambiguous(target, many)),
    }
}

/// Resolve a `--session` target (id, canonical or bare name) to its current entry.
pub fn resolve_target(paths: &Paths, target: &str) -> Result<Session> {
    transaction(paths, |c| {
        let id = target_id(c, target)?;
        Ok(c.sessions.get(&id).context("SESSION_NOT_FOUND")?.clone())
    })
}

/// Set (or clear) an entry's space in a transaction, noting why. A change
/// increments the revision, except that recording a Space where none was known
/// (`unscoped_bumps` false) is not a generation change (rule R1); explicit
/// set-space/move-space pass true.
pub(super) fn assign(
    s: &mut Session,
    notes: &mut BTreeMap<String, &'static str>,
    space: Option<String>,
    source: &'static str,
    unscoped_bumps: bool,
) -> bool {
    if s.space == space {
        return false;
    }
    let first = s.space.is_none() && space.is_some();
    s.space = space;
    if unscoped_bumps || !first {
        s.revision += 1;
    }
    notes.insert(s.id.clone(), source);
    true
}

/// `api session set-space`: catalog only, generation-checked. The caller refuses
/// sessions with a verified-live HERDR terminal (`ALREADY_IN_HERDR`).
pub fn set_space(
    paths: &Paths,
    id: &str,
    generation: &str,
    space: Option<&str>,
) -> Result<Session> {
    if let Some(label) = space {
        crate::paths::validate_space_label(label)?;
    }
    transaction(paths, |c| {
        let current = live(c, id)?;
        ensure!(current.generation() == generation, "STALE_TARGET");
        if current.space.as_deref() == space {
            return Ok(current.clone());
        }
        let name = current.name.clone();
        ensure!(
            !taken(c, space, &name, Some(id), None),
            "SESSION_NAME_EXISTS: \"{name}\" is already used in this Space"
        );
        let s = c.sessions.get_mut(id).context("SESSION_NOT_FOUND")?;
        assign(
            s,
            &mut c.space_notes,
            space.map(str::to_owned),
            "set_space",
            true,
        );
        Ok(s.clone())
    })
}

/// Record a verified destination Space (launch/resume/add-to-herdr completion)
/// without a generation check; a change between two Spaces increments the
/// revision, recording a first Space does not.
pub fn record_space(paths: &Paths, id: &str, space: &str, source: &'static str) -> Result<Session> {
    transaction(paths, |c| {
        live(c, id)?;
        let s = c.sessions.get_mut(id).context("SESSION_NOT_FOUND")?;
        assign(s, &mut c.space_notes, Some(space.to_owned()), source, false);
        Ok(s.clone())
    })
}

/// `record_space` after a verified start: a failure (merged id, I/O, lock) must
/// not turn the successful start into an error, so it is logged and `None` is
/// returned; the next live refresh records the Space.
pub fn record_space_best_effort(
    paths: &Paths,
    id: &str,
    space: &str,
    source: &'static str,
) -> Option<Session> {
    match record_space(paths, id, space, source) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!(
                "space not recorded for {id} ({space}); the next refresh will record it: {e:#}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests;
