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
fn reservation_claims(key: &str, space: Option<&str>, name: &str) -> bool {
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
    transaction(paths, |c| Ok(taken(c, space, name, Some(except), None)))
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
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(t.path().join("data"))).unwrap();
        (t, p)
    }
    fn row(name: &str, space: Option<&str>) -> Session {
        Session {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            provider: "claude".into(),
            account: "fixture".into(),
            conversation_id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            attachment: None,
            directory: Some("/work".into()),
            previous_conversation_ids: vec![],
            space: space.map(str::to_owned),
        }
    }
    fn insert(p: &Paths, s: &Session) {
        transaction(p, |c| {
            c.sessions.insert(s.id.clone(), s.clone());
            Ok(())
        })
        .unwrap();
    }
    fn get(p: &Paths, id: &str) -> Session {
        find(p, id).unwrap()
    }
    fn error<T: std::fmt::Debug>(r: Result<T>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    #[test]
    fn name_grammar() {
        let ok = |input: &str, space: Option<&str>, name: &str| {
            assert_eq!(
                parse_name(input).unwrap(),
                ParsedName { space, name },
                "{input}"
            );
        };
        ok("opus-#1067", None, "opus-#1067");
        ok("localica:opus-#1067", Some("localica"), "opus-#1067");
        ok("Localica:LEAD", Some("Localica"), "LEAD");
        ok("界:名前", Some("界"), "名前");
        ok(&"x".repeat(64), None, &"x".repeat(64));
        for bad in [
            "a:b:c",
            ":lead",
            "localica:",
            "localica: LEAD",
            " localica:LEAD",
            "",
            "a/b",
            "sp/ace:x",
            "..",
            "x\ty",
        ] {
            assert!(
                error(parse_name(bad)).starts_with("INVALID_SESSION_NAME"),
                "{bad:?}"
            );
        }
        assert!(parse_name(&"x".repeat(65)).is_err());
        assert!(parse_name(&format!("s:{}", "x".repeat(65))).is_err());
    }

    #[test]
    fn names_are_unique_per_space_case_insensitively() {
        let (_t, p) = paths();
        let lead = row("LEAD", Some("localica"));
        let other = row("LEAD", Some("sessionstuffs"));
        let third = row("worker", Some("localica"));
        let unscoped = row("worker", None);
        for s in [&lead, &other, &third, &unscoped] {
            insert(&p, s);
        }
        // Same bare name in another Space and in the unscoped bucket: fine.
        assert_eq!(
            get(&p, &other.id).canonical().as_deref(),
            Some("sessionstuffs:LEAD")
        );
        assert_eq!(get(&p, &unscoped.id).canonical(), None);
        // A case variant in the same Space is taken.
        let e = error(rename(&p, &third.id, &third.generation(), "lead"));
        assert!(e.starts_with("SESSION_NAME_EXISTS"), "{e}");
        // Own name with a case change is fine.
        let lead2 = rename(&p, &lead.id, &lead.generation(), "Lead").unwrap();
        assert_eq!(lead2.name, "Lead");
        assert_eq!(lead2.space.as_deref(), Some("localica"));
        // Reservations are scoped too.
        reserve_name(&p, Some("localica"), "new-one", "r1").unwrap();
        assert!(reserve_name(&p, Some("Localica"), "NEW-ONE", "r2").is_err());
        reserve_name(&p, Some("sessionstuffs"), "new-one", "r3").unwrap();
        assert!(check_name(&p, Some("localica"), "lead", "r4").is_err());
        check_name(&p, Some("sessionstuffs"), "worker", "r4").unwrap();
        // complete_reserved checks the request's Space.
        let s = complete_reserved(
            &p,
            "r3",
            Some("sessionstuffs"),
            "new-one",
            ProviderId::Claude,
            "fixture",
            "conversation-r3",
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.canonical().as_deref(), Some("sessionstuffs:new-one"));
        release_reservation(&p, "r1").unwrap();
        reserve_name(&p, Some("localica"), "new-one", "r2").unwrap();
        // free_name suffixes within the Space only.
        assert_eq!(
            free_name(&p, Some("localica"), "worker").unwrap(),
            "worker-2"
        );
        assert_eq!(
            free_name(&p, Some("elsewhere"), "worker").unwrap(),
            "worker"
        );
        assert_eq!(free_name(&p, None, "WORKER").unwrap(), "WORKER-2");
    }

    #[test]
    fn pre_existing_case_collisions_survive_and_stay_addressable() {
        let (_t, p) = paths();
        let upper = row("Foo", Some("main"));
        let lower = row("foo", Some("main"));
        let third = row("bar", Some("main"));
        for s in [&upper, &lower, &third] {
            insert(&p, s);
        }
        assert_eq!(list(&p).unwrap().len(), 3);
        assert!(rename(&p, &third.id, &third.generation(), "FOO").is_err());
        assert_eq!(resolve_target(&p, "main:Foo").unwrap().id, upper.id);
        assert_eq!(resolve_target(&p, "main:foo").unwrap().id, lower.id);
        let e = error(resolve_target(&p, "main:FOO"));
        assert!(e.starts_with("AMBIGUOUS_SESSION: 2 sessions"), "{e}");
    }

    #[test]
    fn targets_resolve_by_id_alias_canonical_and_bare_name() {
        let (_t, p) = paths();
        let a = row("fixture", Some("Main"));
        let b = row("fixture", Some("Other"));
        let c = row("solo", None);
        // A name shaped like an existing id never shadows that id.
        let shadow = row(&a.id, Some("Main"));
        for s in [&a, &b, &c, &shadow] {
            insert(&p, s);
        }
        transaction(&p, |cat| {
            cat.aliases.insert("merged-id".into(), a.id.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(resolve_target(&p, &a.id).unwrap().id, a.id);
        assert!(error(resolve_target(&p, "merged-id")).starts_with("SESSION_MERGED"));
        assert_eq!(resolve_target(&p, "Main:fixture").unwrap().id, a.id);
        assert_eq!(resolve_target(&p, "main:fixture").unwrap().id, a.id);
        assert_eq!(resolve_target(&p, "Other:fixture").unwrap().id, b.id);
        assert_eq!(resolve_target(&p, "solo").unwrap().id, c.id);
        assert_eq!(resolve_target(&p, "SOLO").unwrap().id, c.id);
        let e = error(resolve_target(&p, "fixture"));
        assert!(e.starts_with("AMBIGUOUS_SESSION"), "{e}");
        assert!(
            e.contains("Main:fixture") && e.contains("Other:fixture"),
            "{e}"
        );
        assert!(error(resolve_target(&p, "missing")).starts_with("SESSION_NOT_FOUND"));
        assert!(error(resolve_target(&p, "Nowhere:fixture")).starts_with("SESSION_NOT_FOUND"));
        // Two host-observed duplicates in one Space: the canonical form is ambiguous.
        let d = row("fixture", Some("Other"));
        insert(&p, &d);
        assert!(error(resolve_target(&p, "Other:fixture")).starts_with("AMBIGUOUS_SESSION"));
    }

    #[test]
    fn rename_never_moves_and_keeps_the_space() {
        let (_t, p) = paths();
        let scoped = row("old", Some("localica"));
        let unscoped = row("legacy", None);
        insert(&p, &scoped);
        insert(&p, &unscoped);
        let renamed = rename(&p, &scoped.id, &scoped.generation(), "LEAD").unwrap();
        assert_eq!(renamed.space.as_deref(), Some("localica"));
        let same = rename(&p, &scoped.id, &renamed.generation(), "Localica:LEAD").unwrap();
        assert_eq!(
            same.generation(),
            renamed.generation(),
            "byte-equal name is a no-op"
        );
        let moved = rename(&p, &scoped.id, &renamed.generation(), "localica:opus-#1");
        assert_eq!(
            moved.unwrap().canonical().as_deref(),
            Some("localica:opus-#1")
        );
        let current = get(&p, &scoped.id);
        let e = error(rename(
            &p,
            &scoped.id,
            &current.generation(),
            "sessionstuffs:x",
        ));
        assert!(
            e.starts_with("SPACE_MISMATCH") && e.contains("move-space"),
            "{e}"
        );
        let e = error(rename(
            &p,
            &unscoped.id,
            &unscoped.generation(),
            "localica:x",
        ));
        assert!(
            e.starts_with("SPACE_MISMATCH") && e.contains("set-space"),
            "{e}"
        );
        let bare = rename(&p, &unscoped.id, &unscoped.generation(), "fable-glass").unwrap();
        assert_eq!((bare.space.clone(), bare.canonical()), (None, None));
    }

    #[test]
    fn set_space_is_scoped_generation_checked_and_clearable() {
        let (_t, p) = paths();
        let s = row("worker", None);
        let taken = row("worker", Some("localica"));
        insert(&p, &s);
        insert(&p, &taken);
        let e = error(set_space(&p, &s.id, &s.generation(), Some("localica")));
        assert!(e.starts_with("SESSION_NAME_EXISTS"), "{e}");
        assert!(
            error(set_space(&p, &s.id, &s.generation(), Some("a/b")))
                .starts_with("INVALID_SPACE_LABEL")
        );
        let set = set_space(&p, &s.id, &s.generation(), Some("closed-space")).unwrap();
        assert_eq!(set.canonical().as_deref(), Some("closed-space:worker"));
        assert_ne!(
            set.generation(),
            s.generation(),
            "an explicit change is a new generation"
        );
        assert!(error(set_space(&p, &s.id, &s.generation(), None)).starts_with("STALE_TARGET"));
        let cleared = set_space(&p, &s.id, &set.generation(), None).unwrap();
        assert_eq!(cleared.space, None);
        // An explicit clear is recorded, so it is not "healed" back.
        assert_eq!(get(&p, &s.id).space, None);
    }

    fn write(p: &Paths, file: &str, value: serde_json::Value) {
        let path = p.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    /// A catalog written by 1.47 (no `space`, no sidecar), plus its evidence files.
    fn legacy_root() -> (tempfile::TempDir, Paths, Vec<Session>, Vec<u8>) {
        let (t, p) = paths();
        let rows: Vec<Session> = ["placed", "recovered", "both", "none"]
            .iter()
            .map(|n| row(n, None))
            .collect();
        let catalog = serde_json::json!({"schema_version":1,
            "sessions": rows.iter().map(|s| (s.id.clone(), serde_json::to_value(s).unwrap())).collect::<serde_json::Map<_,_>>(),
            "reservations":{}});
        let bytes = serde_json::to_vec(&catalog).unwrap();
        write(&p, "registry/session-catalog/catalog.json", catalog);
        let evidence = |label: &str| {
            serde_json::json!({"attachment":{"host":"herdr","target":"herdr/s/w1:p1","pid":1,"process_start":1},
            "host_id":"h","space":{"id":"w1","label":label,"observed_at":1},"observed_at":1,"checked_at":1,"reason":null})
        };
        write(
            &p,
            "registry/session-catalog/placements.json",
            serde_json::json!({"hosts":{},"sessions":{
                rows[0].id.clone(): evidence("localica"),
                rows[2].id.clone(): evidence("sessionstuffs")}}),
        );
        write(
            &p,
            "registry/auto-recover.json",
            serde_json::json!({"schema_version":1,"sessions":{
                rows[1].id.clone(): {"enabled":true,"workspace_label":"liq-glass"},
                rows[2].id.clone(): {"enabled":true,"workspace_label":"ignored"}}}),
        );
        (t, p, rows, bytes)
    }

    #[test]
    fn backfill_never_changes_a_generation() {
        let (_t, p, rows, _) = legacy_root();
        let after = list(&p).unwrap();
        let by = |id: &str| after.iter().find(|s| s.id == id).unwrap().clone();
        assert_eq!(by(&rows[0].id).space.as_deref(), Some("localica"));
        assert_eq!(by(&rows[1].id).space.as_deref(), Some("liq-glass"));
        assert_eq!(by(&rows[2].id).space.as_deref(), Some("sessionstuffs"));
        assert_eq!(by(&rows[3].id).space, None);
        for before in &rows {
            let now = by(&before.id);
            // Byte-identical generation and revision: deploying 1.48.0 moves no binding.
            assert_eq!(now.generation(), before.generation(), "{}", before.name);
            assert_eq!(now.revision, before.revision);
            assert_eq!(now.name, before.name);
        }
        let sidecar = read_sidecar(&p.root.join("registry/session-catalog/spaces.json")).unwrap();
        assert!(sidecar.backfilled_at.is_some());
        assert_eq!(sidecar.sessions[&rows[0].id].source, "placement_evidence");
        assert_eq!(sidecar.sessions[&rows[1].id].source, "auto_recover");
        assert_eq!(sidecar.sessions[&rows[3].id].source, "backfill_null");
        // A second run is a no-op, even with new evidence for the null row.
        write(
            &p,
            "registry/auto-recover.json",
            serde_json::json!({"sessions":{rows[3].id.clone(): {"workspace_label":"late"}}}),
        );
        let again = list(&p).unwrap();
        assert_eq!(
            again.iter().find(|s| s.id == rows[3].id).unwrap().space,
            None
        );
        assert_eq!(
            read_sidecar(&p.root.join("registry/session-catalog/spaces.json")).unwrap(),
            sidecar
        );
    }

    #[test]
    fn a_rollback_rewrite_is_healed_with_unchanged_generations() {
        let (_t, p, rows, legacy) = legacy_root();
        let scoped = list(&p).unwrap();
        // A 1.47 binary rewrites catalog.json without `space` (serde drops the field).
        #[derive(Serialize, Deserialize)]
        struct OldSession {
            id: String,
            name: String,
            provider: String,
            account: String,
            conversation_id: String,
            revision: u64,
            attachment: Option<Attachment>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            directory: Option<String>,
        }
        let file = p.root.join("registry/session-catalog/catalog.json");
        let current: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        let mut old = current.clone();
        for (id, s) in current["sessions"].as_object().unwrap() {
            let o: OldSession = serde_json::from_value(s.clone()).unwrap();
            old["sessions"][id] = serde_json::to_value(o).unwrap();
        }
        fs::write(&file, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&legacy).unwrap()["sessions"],
            old["sessions"]
        );
        let healed = repair_identity(&p).unwrap();
        assert_eq!(healed.restored_spaces.len(), 3);
        let after = list(&p).unwrap();
        for s in &scoped {
            let now = after.iter().find(|x| x.id == s.id).unwrap();
            assert_eq!(now.space, s.space);
            assert_eq!(now.generation(), s.generation());
        }
        assert!(rows.iter().all(|r| after.iter().any(|s| s.id == r.id)));
        // Crash between the sidecar and the catalog write: the stale catalog heals too.
        let s = after.iter().find(|s| s.id == rows[3].id).unwrap();
        let set = set_space(&p, &s.id, &s.generation(), Some("temp")).unwrap();
        fs::write(&file, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(get(&p, &set.id).space.as_deref(), Some("temp"));
    }

    #[test]
    fn an_inactive_session_keeps_its_space() {
        let (_t, p) = paths();
        let s = record_launch(&p, "fixture", "thread-1", "LEAD", None).unwrap();
        let s = set_space(&p, &s.id, &s.generation(), Some("localica")).unwrap();
        // Detach / stop, a reconcile that finds nothing, a merge-free relink, an
        // account move and an attachment clear never touch the Space.
        let s = invalidate(&p, &s.id, &s.generation()).unwrap();
        reconcile(&p, &[]).unwrap();
        reassign_account(&p, "codex", "fixture", "other").unwrap();
        let moved = move_attachment(&p, &get(&p, &s.id), None, None).unwrap();
        assert_eq!(moved.space.as_deref(), Some("localica"));
        // A backfill rerun (sidecar lost its marker) never nulls a stored Space.
        let file = p.root.join("registry/session-catalog/spaces.json");
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("backfilled_at");
        fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
        let now = get(&p, &s.id);
        assert_eq!(now.canonical().as_deref(), Some("localica:LEAD"));
        // An explicit move-space records the verified destination.
        let moved = move_attachment(&p, &now, None, Some("sessionstuffs".into())).unwrap();
        assert_eq!(moved.canonical().as_deref(), Some("sessionstuffs:LEAD"));
        assert_ne!(moved.generation(), now.generation());
    }

    #[test]
    fn a_resume_rename_moves_into_the_destination_space() {
        let (_t, p) = paths();
        let s = row("x", Some("Main"));
        let clash = row("y", Some("Other"));
        insert(&p, &s);
        insert(&p, &clash);
        let e = error(rename_in_space(
            &p,
            &s.id,
            &s.generation(),
            "Y",
            Some("other"),
            "launch",
        ));
        assert!(e.starts_with("SESSION_NAME_EXISTS"), "{e}");
        let moved =
            rename_in_space(&p, &s.id, &s.generation(), "y", Some("Main"), "launch").unwrap();
        assert_eq!(moved.canonical().as_deref(), Some("Main:y"));
        let r = record_space(&p, &s.id, "Main", "launch").unwrap();
        assert_eq!(
            r.generation(),
            moved.generation(),
            "recording the same Space is no change"
        );
    }

    #[test]
    fn recording_a_first_space_keeps_the_generation_but_a_move_changes_it() {
        let (_t, p) = paths();
        let s = row("x", None);
        insert(&p, &s);
        let first = record_space(&p, &s.id, "A", "launch").unwrap();
        assert_eq!(first.space.as_deref(), Some("A"));
        assert_eq!(
            first.generation(),
            s.generation(),
            "null -> Space is R1-neutral"
        );
        let moved = record_space(&p, &s.id, "B", "launch").unwrap();
        assert_eq!(moved.space.as_deref(), Some("B"));
        assert_ne!(moved.generation(), first.generation(), "A -> B changes it");
        // Explicit set-space from no Space still changes the generation.
        let u = row("u", None);
        insert(&p, &u);
        let set = set_space(&p, &u.id, &u.generation(), Some("A")).unwrap();
        assert_ne!(set.generation(), u.generation());
    }

    #[test]
    fn a_failed_space_record_after_a_start_is_not_an_error() {
        let (_t, p) = paths();
        // Unknown id: record_space fails; the best-effort form logs and returns None.
        assert!(record_space(&p, "missing", "A", "launch").is_err());
        assert!(record_space_best_effort(&p, "missing", "A", "launch").is_none());
        let s = row("x", None);
        insert(&p, &s);
        assert!(record_space_best_effort(&p, &s.id, "A", "launch").is_some());
    }

    #[test]
    fn automatic_names_avoid_every_space() {
        let (_t, p) = paths();
        insert(&p, &row("Work", Some("A")));
        let made = record_launch(&p, "fixture", "thread-auto", "work", None).unwrap();
        assert_eq!(made.name, "work-2");
    }

    #[test]
    fn canonical_is_null_for_a_label_with_a_colon() {
        let s = row("x", Some("a:b"));
        assert_eq!(s.canonical(), None);
        assert_eq!(row("x", Some("ab")).canonical().as_deref(), Some("ab:x"));
    }
}
