//! One session: the re-key of a stopped Claude session (§4.2), the Tier S
//! worker for a running one (§4.5-4.6), Codex thread locations (§5) and the
//! per-session rollback (§9).
use super::*;
use crate::background::SpawnReaped as _;
use std::time::{Duration, Instant};

pub(crate) struct Ctx<'a> {
    pub paths: &'a Paths,
    pub backup: PathBuf,
    pub allow_full_copy: bool,
}
impl<'a> Ctx<'a> {
    pub fn new(paths: &'a Paths, backup: &Path, allow_full_copy: bool) -> Self {
        Self {
            paths,
            backup: backup.to_path_buf(),
            allow_full_copy,
        }
    }
}

const PHASES: &[&str] = &[
    "admitted",
    "stopping",
    "stopped",
    "retained",
    "moved",
    "switched",
    "resume_pending",
    "resuming",
    "verifying",
    "complete",
];
fn order(p: &str) -> usize {
    PHASES.iter().position(|x| *x == p).unwrap_or(0)
}
fn phase(paths: &Paths, r: &mut SessionRec, name: &str, detail: &str) -> Result<()> {
    r.phase = name.into();
    r.detail = detail.into();
    save_session(paths, r)?;
    #[cfg(test)]
    super::tests::die_check(name);
    #[cfg(test)]
    super::tests::crash_check(name)?;
    Ok(())
}
/// The account home of the session (fixed in the record at admission).
fn home_of(paths: &Paths, r: &SessionRec) -> Result<PathBuf> {
    r.home
        .clone()
        .or_else(|| claude_home(paths, &r.account))
        .context("ACCOUNT_UNAVAILABLE")
}
fn projects_of(paths: &Paths, r: &SessionRec) -> Result<PathBuf> {
    Ok(home_of(paths, r)?.join("projects"))
}
fn bases_entry(paths: &Paths, km: &str, kt: &str) -> (keys::Bases, String) {
    (read_bases(paths), keys::pair(km, kt))
}
/// Where replaced memory versions go when no retained directory is known yet.
fn memory_backup(paths: &Paths, r: &SessionRec) -> PathBuf {
    r.retained_dir
        .clone()
        .or_else(|| latest_run(paths).map(|run| run.backup.join("sessions").join(&r.session_id)))
        .unwrap_or_else(|| dir(paths).join("memory-versions-fallback"))
}

/// B1: fails CLOSED. No verified live process holds a lineage id in the
/// session's home or any enrolled Claude home, no process names a lineage id
/// on its command line, and no transcript has a writer (compared by its
/// resolved path). Any probe error refuses with `SESSION_STATE_UNVERIFIED:
/// probe_failed`.
pub(crate) fn ensure_quiet(paths: &Paths, r: &SessionRec, found: &[Found]) -> Result<()> {
    let mut homes: Vec<PathBuf> = claude::enrolled(paths)
        .map_err(|e| {
            anyhow::anyhow!("SESSION_STATE_UNVERIFIED: probe_failed: enrolled accounts: {e:#}")
        })?
        .into_iter()
        .map(|a| a.home)
        .collect();
    if let Ok(h) = home_of(paths, r)
        && !homes.iter().any(|x| crate::layout::same_home(x, &h))
    {
        homes.push(h);
    }
    for home in homes {
        let live = claude::live_sessions(&home).map_err(|e| {
            anyhow::anyhow!(
                "SESSION_STATE_UNVERIFIED: probe_failed: live sessions of {}: {e:#}",
                home.display()
            )
        })?;
        if let Some(l) = live
            .iter()
            .find(|l| r.conversation_ids.contains(&l.session_id))
        {
            bail!(
                "SESSION_RUNNING: Claude process {} runs conversation {}; it was not moved",
                l.pid,
                l.session_id
            );
        }
    }
    let naming = processes_naming(&r.conversation_ids)
        .map_err(|e| anyhow::anyhow!("SESSION_STATE_UNVERIFIED: probe_failed: {e}"))?;
    ensure!(
        naming.is_empty(),
        "SESSION_RUNNING: process(es) {naming:?} name this conversation on their command line; it was not moved"
    );
    for f in found {
        // After an interrupted move the transcript may already be in the new key.
        let moved_to = r
            .key_to
            .as_ref()
            .zip(projects_of(paths, r).ok())
            .map(|(kt, p)| p.join(kt).join(format!("{}.jsonl", f.conversation_id)));
        let at = if fs::symlink_metadata(&f.transcript).is_ok() {
            f.transcript.clone()
        } else if let Some(m) = moved_to.filter(|m| fs::symlink_metadata(m).is_ok()) {
            m
        } else {
            continue; // nothing to probe: the name is in neither key
        };
        crate::external::archiving::native::not_open(&at).map_err(|e| {
            if format!("{e:#}").contains("HISTORY_IN_USE") {
                e
            } else {
                anyhow::anyhow!("SESSION_STATE_UNVERIFIED: probe_failed: open files: {e:#}")
            }
        })?;
    }
    Ok(())
}

/// Steps 1-6 of §4.2 for a session that is not running (Tier R, or the worker
/// once it stopped the process). Phase-ordered and resumable.
pub(crate) fn rekey_stopped(ctx: &mut Ctx, r: &mut SessionRec, s: &Session) -> Result<()> {
    rekey_until(ctx, r, s, true)
}
/// `finish`: end in `complete` / `rekeyed` (no resume follows); else stop at `switched`.
fn rekey_until(ctx: &mut Ctx, r: &mut SessionRec, s: &Session, finish: bool) -> Result<()> {
    let paths = ctx.paths;
    let from = r.from.clone().context("SAVED_DIRECTORY_UNAVAILABLE")?;
    let to = r.to.clone().context("SAVED_DIRECTORY_UNAVAILABLE")?;
    let key_to = keys::key(&to);
    let mem_key = keys::key(&from);
    r.key_to = Some(key_to.clone());
    r.memory_key = Some(mem_key.clone());
    ensure!(
        key_to.len() <= keys::KEY_MAX,
        "ROOT_REKEY_UNSUPPORTED: the new project key would be longer than {} characters",
        keys::KEY_MAX
    );
    ensure!(
        Path::new(&to).is_dir(),
        "ROOT_REKEY_UNSUPPORTED: {to} is not a directory"
    );
    let projects = projects_of(paths, r)?;
    if r.home.is_none() {
        r.home = Some(home_of(paths, r)?);
    }
    if order(&r.phase) < order("retained") {
        // 1. Locate, 2. quiet check, 4. collision preflight (before anything moves).
        let found = keys::locate(&projects, &r.conversation_ids)?;
        r.key_from = found
            .iter()
            .find(|f| f.conversation_id == r.conversation_id)
            .or(found.first())
            .map(|f| f.key.clone());
        ensure_quiet(paths, r, &found)?;
        r.collisions = keys::collisions(&projects, &found, &key_to);
        ensure!(
            r.collisions.is_empty(),
            "ROOT_REKEY_CONFLICT: {} already exist(s) in the target key; nothing was changed",
            r.collisions.join(", ")
        );
        // Memory preflight: a conflict blocks before any file moves.
        let (bases, pair) = bases_entry(paths, &mem_key, &key_to);
        let mut base = bases.0.get(&pair).cloned().unwrap_or_default();
        let pre = keys::sync_memory(
            &projects.join(&mem_key).join("memory"),
            &projects.join(&key_to).join("memory"),
            &mut base,
            Path::new(""), // dry run: writes nothing
            &key_to,
            true,
        )?;
        if !pre.clean() && mem_key != key_to {
            r.memory = Some(pre.clone());
            bail!(
                "MEMORY_CONFLICT: {} differ(s) between the old and new key's memory; nothing was moved",
                pre.conflicts.join(", ")
            );
        }
        // 3. Retain: copy + sha256-verify outside projects/ BEFORE any rename.
        r.attempt += 1;
        let dir = ctx
            .backup
            .join("sessions")
            .join(&r.session_id)
            .join(r.attempt.to_string());
        r.retained_dir = Some(dir.clone());
        r.found = found.clone();
        save_session(paths, r)?;
        #[cfg(test)]
        super::tests::die_check("before_copy");
        r.retained = keys::retain(&found, &dir, ctx.allow_full_copy)?;
        phase(
            paths,
            r,
            "retained",
            "Every file of the conversation was copied to the backup directory and verified.",
        )?;
    }
    if order(&r.phase) < order("moved") {
        ensure_quiet(paths, r, &r.found.clone())?;
        // 4. Move (RENAME_NOREPLACE) while stopped; the retained copy exists and matched.
        ensure!(
            r.retained.iter().all(|x| x.verified),
            "RETAIN_VERIFY_FAILED: the retained copy is not verified; nothing was moved"
        );
        // B1 (review 2): every rename is journaled (intent before, done after).
        let retained = r.retained.clone();
        let moved = keys::move_names(&projects, &retained, &key_to, |m| {
            r.moved = m.to_vec();
            save_session(paths, r)
        })?;
        r.moved = moved;
        phase(
            paths,
            r,
            "moved",
            "The conversation's files were moved to the new project key.",
        )?;
        if test_fault("exit_after_moved") {
            // Process death after the move was journaled (binary-level crash tests).
            std::process::exit(86);
        }
    }
    if order(&r.phase) < order("switched") {
        // B3(b): any error from here on moves the names back (and restores the
        // catalog when it was switched) before it is reported.
        if let Err(e) = memory_and_switch(ctx, r, s, &projects, &from, &to, &mem_key, &key_to) {
            let back = undo_moves(paths, r);
            return Err(match back {
                Ok(()) => e,
                Err(b) => e.context(format!("ROOT_REKEY_REVERT_FAILED: {b:#}")),
            });
        }
    }
    if !finish {
        return Ok(());
    }
    r.state = "rekeyed".into();
    r.reason = None;
    phase(
        paths,
        r,
        "complete",
        "Re-keyed; verified at its next resume.",
    )
}

/// Fault injection for the binary-level tests (debug builds only, like the
/// symlink test proof; release builds never read the variable).
fn test_fault(point: &str) -> bool {
    cfg!(debug_assertions)
        && std::env::var("SESSIONSATION_TEST_FAULT").is_ok_and(|v| v.split(',').any(|p| p == point))
}

/// Steps 5 and 6 of §4.2 (after the names moved): memory, project entry, catalog.
#[allow(clippy::too_many_arguments)]
fn memory_and_switch(
    ctx: &mut Ctx,
    r: &mut SessionRec,
    s: &Session,
    projects: &Path,
    from: &str,
    to: &str,
    mem_key: &str,
    key_to: &str,
) -> Result<()> {
    let paths = ctx.paths;
    // An I/O error after the move (B3(b)).
    #[cfg(test)]
    super::tests::crash_check("memory_io")?;
    if test_fault("memory_conflict") {
        bail!(
            "MEMORY_CONFLICT: MEMORY.md differ(s) between the old and new key's memory (injected)"
        );
    }
    if test_fault("stale_target") {
        bail!("STALE_TARGET (injected)");
    }
    if test_fault("io_error") {
        return Err(std::io::Error::other("injected I/O error").into());
    }
    if mem_key != key_to {
        let (mut bases, pair) = bases_entry(paths, mem_key, key_to);
        let base = bases.0.entry(pair).or_default();
        let m = keys::sync_memory(
            &projects.join(mem_key).join("memory"),
            &projects.join(key_to).join("memory"),
            base,
            &ctx.backup,
            key_to,
            false,
        )?;
        save_bases(paths, &bases)?;
        r.memory = Some(m.clone());
        #[cfg(test)]
        super::tests::die_check("memory_synced");
        ensure!(
            m.clean(),
            "MEMORY_CONFLICT: {} differ(s) between the old and new key's memory; the files were moved back",
            m.conflicts.join(", ")
        );
    }
    for home in claude::enrolled(paths)
        .map(|e| e.into_iter().map(|a| a.home).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let alias = Alias::new(vec![Entry {
            from: from.into(),
            to: to.into(),
            proof: "session".into(),
        }]);
        let _ = keys::clone_project_entries(&home, &alias, Some(from), false);
    }
    // 6. Switch the catalog (generation-checked against the stored directory).
    let (before, after) =
        catalog::rekey_directory(paths, &s.id, &s.conversation_id, Some(from), to)?;
    r.revision_before.get_or_insert(before);
    r.revision_after = Some(after.revision);
    // Process death between the catalog switch and the journal write (B3(e)).
    #[cfg(test)]
    super::tests::die_check("catalog_switched");
    phase(
        paths,
        r,
        "switched",
        "The catalog directory now names the new spelling.",
    )
}

/// Move the names back and restore the catalog directory (and revision) if it
/// was switched. The record keeps `moved` until the move-back succeeded.
pub(crate) fn undo_moves(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    keys::move_back(&r.moved)?;
    r.moved.clear();
    restore_catalog(paths, r)?;
    r.phase = "admitted".into();
    save_session(paths, r)
}
/// Point the catalog back at the old directory when it names the new one.
/// The journal may lack the revisions (death between the catalog switch and
/// the journal write): the directory is restored all the same (a new revision).
fn restore_catalog(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    if let Some(to) = r.to.clone() {
        let (before, after) = (
            r.revision_before.unwrap_or(u64::MAX),
            r.revision_after.unwrap_or(u64::MAX),
        );
        catalog::restore_directory(
            paths,
            &r.session_id,
            &r.conversation_id,
            &to,
            r.from.as_deref(),
            before,
            after,
        )?;
    }
    r.revision_after = None;
    Ok(())
}

/// Record a re-key that failed: its files are back (B3(b)) and it stays on the
/// old path (`left_behind`), or, when moving back failed too, `uncertain` with
/// `moved` kept so `root rollback` can finish it.
pub(crate) fn abandon_rekey(paths: &Paths, r: &mut SessionRec, e: &anyhow::Error) -> Result<()> {
    let back = if r.moved.is_empty() && r.revision_after.is_none() {
        Ok(())
    } else {
        undo_moves(paths, r)
    };
    r.reason = Some(format!("{e:#}"));
    match back {
        Ok(()) => {
            r.state = "left_behind".into();
            r.phase = "complete".into();
        }
        Err(b) => {
            r.state = "uncertain".into();
            r.phase = "uncertain".into();
            r.uncertain_from = Some("moved".into());
            r.warnings.push(format!("move back failed: {b:#}"));
        }
    }
    save_session(paths, r)
}

/// Undo a stopped session's re-key: move the files back, merge memory in the
/// reverse direction, restore the catalog directory (and its revision).
pub(crate) fn rollback_stopped(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    undo_session(paths, r)?;
    r.state = "not_applicable".into();
    r.reason = Some("rolled_back".into());
    r.phase = "complete".into();
    r.rollback = false;
    r.detail = "Rolled back to the old path; the retained copy is kept.".into();
    save_session(paths, r)
}
/// Names in `K_new` that belong to this session but are not (or not yet) in the
/// journal's move list: a retained name whose source is gone and whose copy in
/// `K_new` has the retained digest (a rename the journal missed), and, for
/// rollback (S6), any `<id>.jsonl` / `<id>/` of the lineage created in `K_new`
/// after the resume, when the old key does not hold that name.
pub(crate) fn untracked_moves(projects: &Path, r: &SessionRec) -> Vec<Moved> {
    let mut out = vec![];
    let Some(kt) = r.key_to.clone() else {
        return out;
    };
    let known =
        |name: &str, out: &[Moved]| r.moved.iter().chain(out.iter()).any(|m| m.name == name);
    for x in &r.retained {
        if x.key == kt || known(&x.name, &out) {
            continue;
        }
        let to = projects.join(&kt).join(&x.name);
        let digest = if x.dir {
            crate::external::archiving::native::tree_digest(&to).map(|d| d.1)
        } else {
            keys::sha256_file(&to).map(|d| d.1)
        };
        if fs::symlink_metadata(&x.source).is_err() && digest.is_ok_and(|d| d == x.sha256) {
            out.push(Moved {
                name: x.name.clone(),
                from: x.source.clone(),
                to,
                digest: x.sha256.clone(),
            });
        }
    }
    if let Some(kf) = r.key_from.clone().filter(|kf| *kf != kt) {
        for id in &r.conversation_ids {
            for name in [format!("{id}.jsonl"), id.clone()] {
                let (to, from) = (
                    projects.join(&kt).join(&name),
                    projects.join(&kf).join(&name),
                );
                if !known(&name, &out)
                    && fs::symlink_metadata(&to).is_ok()
                    && fs::symlink_metadata(&from).is_err()
                {
                    out.push(Moved {
                        name,
                        from,
                        to,
                        digest: String::new(),
                    });
                }
            }
        }
    }
    out
}

/// The file, memory, catalog and thread-location part of a rollback.
fn undo_session(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    match rekey_of(&r.provider) {
        Some((_, Rekey::ClaudeProjects)) => {
            // Review 3 S7: conversation ids continued after the resume belong to
            // the session too (current catalog lineage), for the sweep and the
            // quiet check.
            if let Ok(s) = catalog::find(paths, &r.session_id) {
                for id in s
                    .previous_conversation_ids
                    .iter()
                    .chain(std::iter::once(&s.conversation_id))
                {
                    if !r.conversation_ids.contains(id) {
                        r.conversation_ids.push(id.clone());
                    }
                }
            }
            if let Ok(projects) = projects_of(paths, r) {
                let extra = untracked_moves(&projects, r);
                if !extra.is_empty() {
                    r.warnings.push(format!(
                        "rollback: {} name(s) found in the new key that the journal did not list were moved back",
                        extra.len()
                    ));
                    r.moved.extend(extra);
                    save_session(paths, r)?;
                }
            }
            // B3(a): whatever the reported state, files the journal shows moved go back.
            let switched = r.revision_after.is_some();
            if !r.moved.is_empty() || switched {
                let projects = projects_of(paths, r)?;
                let found: Vec<Found> = vec![];
                ensure_quiet(paths, r, &found)?;
                if let Err(e) = keys::move_back(&r.moved) {
                    // Never a reported success: the session needs a person.
                    r.state = "uncertain".into();
                    r.phase = "uncertain".into();
                    r.reason = Some(format!("{e:#}"));
                    save_session(paths, r)?;
                    return Err(e);
                }
                r.moved.clear();
                save_session(paths, r)?;
                if let (Some(km), Some(kt)) = (r.memory_key.clone(), r.key_to.clone())
                    && km != kt
                {
                    let mut bases = read_bases(paths);
                    let base = bases.0.entry(keys::pair(&km, &kt)).or_default();
                    let m = keys::sync_memory(
                        &projects.join(&kt).join("memory"),
                        &projects.join(&km).join("memory"),
                        base,
                        &memory_backup(paths, r),
                        &km,
                        false,
                    )?;
                    save_bases(paths, &bases)?;
                    if !m.clean() {
                        r.warnings.push(format!(
                            "rollback: memory conflict(s) kept in place: {}",
                            m.conflicts.join(", ")
                        ));
                    }
                }
            }
            restore_catalog(paths, r)?;
        }
        None if r.codex_location_written => {
            let file = paths
                .root
                .join(layout::THREAD_LOCATIONS)
                .join(format!("{}.json", r.conversation_id));
            let current = fs::read_to_string(&file).ok();
            // Item 4: only the record this tool wrote is restored or removed.
            if current.is_some() && current == r.codex_location_after {
                match &r.codex_location_before {
                    Some(text) => atomic_write(&file, text.as_bytes())?,
                    None => fs::remove_file(&file)?,
                }
            } else if current != r.codex_location_before {
                r.warnings.push(
                    "rollback: the thread location changed since the re-key; it was left as is"
                        .into(),
                );
            }
        }
        _ => {}
    }
    r.moved.clear();
    r.revision_after = None;
    r.codex_location_written = false;
    save_session(paths, r)
}

/// Codex thread that is not running: record its location in the new spelling.
pub(crate) fn codex_rekey_stopped(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    let to = r.to.clone().context("SAVED_DIRECTORY_UNAVAILABLE")?;
    let file = paths
        .root
        .join(layout::THREAD_LOCATIONS)
        .join(format!("{}.json", r.conversation_id));
    // S1 (review 2): `before` is read once and journaled, together with the exact
    // record about to be written, BEFORE the write; a rerun never re-reads it.
    if !r.codex_location_written {
        r.codex_location_before = fs::read_to_string(&file).ok();
        r.codex_location_after = Some(String::from_utf8(serde_json::to_vec(
            &json!({"cwd": Path::new(&to)}),
        )?)?);
        r.codex_location_written = true;
        save_session(paths, r)?;
    }
    crate::external::relocation::save_location(paths, &r.conversation_id, Path::new(&to))?;
    ensure!(
        fs::read_to_string(&file).ok() == r.codex_location_after,
        "ROOT_VERIFY_FAILED: the thread location record differs from the journaled one"
    );
    Ok(())
}

// ------------------------------------------------------------------ Tier S

#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    pub session: String,
    pub expected_generation: Option<String>,
    pub accept_risks: bool,
    pub prompt: Option<String>,
}

fn active_run(paths: &Paths) -> Result<Run> {
    let record = layout::read_record(&paths.root)?;
    ensure!(
        record
            .as_ref()
            .and_then(|r| r.root_alias.as_ref())
            .is_some_and(RootAlias::is_active),
        "ROOT_MIGRATION_MISMATCH: no active root alias; run `root migrate --from F --to T --backup DIR` first"
    );
    latest_run(paths).context("ROOT_MIGRATION_MISMATCH: the run journal is missing")
}

/// `root migrate --session S`.
pub async fn migrate_session(paths: &Paths, o: &SessionOptions) -> Result<Value> {
    let run = active_run(paths)?;
    let alias = Alias::new(run.entries.clone());
    let s = catalog::resolve_target(paths, &o.session)?;
    if let Some(g) = &o.expected_generation {
        ensure!(s.generation() == *g, "STALE_TARGET");
    }
    let mut r = read_session(paths, &s.id).unwrap_or_else(|| new_rec(&s, &run.id));
    if r.state == "verified" {
        return Ok(json!({"session":row(&r),"noop":true}));
    }
    match rekey_of(&s.provider) {
        Some((_, Rekey::Unsupported)) => bail!(
            "ROOT_REKEY_UNSUPPORTED_PROVIDER: {} sessions are not re-keyed in 1.54; it stays on its old path",
            s.provider
        ),
        Some((_, Rekey::ClaudeProjects)) => {}
        _ => return migrate_codex(paths, &run, &alias, s, r, o).await,
    }
    if archived_ids(paths).contains(&s.id) {
        if let Some(dir) = s.directory.as_deref() {
            let to = alias.logical(Path::new(dir)).to_string_lossy().into_owned();
            if to != dir {
                let (before, after) =
                    catalog::rekey_directory(paths, &s.id, &s.conversation_id, Some(dir), &to)?;
                r.revision_before.get_or_insert(before);
                r.revision_after = Some(after.revision);
                r.to = Some(to);
            }
        }
        r.state = "archived_rekey_at_restore".into();
        r.phase = "complete".into();
        save_session(paths, &mut r)?;
        return Ok(json!({"session":row(&r)}));
    }
    ensure!(
        !claude::hosted::live(paths, &s.conversation_id),
        "SESSION_HOSTED: a Sessionsation-hosted (stream-json) Claude session is not re-keyed; stop it first"
    );
    if r.phase == "uncertain"
        || (r.phase != "complete"
            && r.phase != "failed"
            && ((r.process.is_none() && !r.rollback)
                || (r.phase != "admitted"
                    && r.phase != "rollback_admitted"
                    && worker_gone(paths, &r))))
    {
        // Continue from the journaled phase (B3(e)): a stopped session's re-key
        // continues inline, a running one's in its worker.
        if let Some(from) = r.uncertain_from.take() {
            r.phase = from;
        }
        if r.rollback {
            save_session(paths, &mut r)?;
            spawn(paths, &r)?;
            return Ok(json!({"session":row(&r),"continued":true}));
        }
        if r.process.is_none() {
            let _guard = crate::lifecycle::admit(paths, &s.conversation_id, Some(&s.id))?;
            let mut ctx = Ctx::new(paths, &run.backup, run.allow_full_copy);
            r.state = "rekeying".into();
            if let Err(e) = rekey_stopped(&mut ctx, &mut r, &s) {
                abandon_rekey(paths, &mut r, &e)?;
                bail!("{e:#}");
            }
            return Ok(json!({"session":row(&r),"continued":true}));
        }
        save_session(paths, &mut r)?;
        spawn(paths, &r)?;
        return Ok(json!({"session":row(&r),"continued":true}));
    }
    retry_allowed(&r)?;
    ensure!(
        r.phase == "complete" || r.phase == "failed",
        "SESSION_OPERATION_IN_PROGRESS: the re-key of this session is in phase {}",
        r.phase
    );
    let from = if matches!(r.state.as_str(), "rekeyed") {
        return Ok(
            json!({"session":row(&r),"noop":true,"detail":"re-keyed; verified at its next resume"}),
        );
    } else {
        s.directory.clone().context("SAVED_DIRECTORY_UNAVAILABLE")?
    };
    let to = alias
        .logical(Path::new(&from))
        .to_string_lossy()
        .into_owned();
    ensure!(
        to != from,
        "ROOT_REKEY_UNSUPPORTED: {from} is not under an alias entry"
    );
    let home = claude_home(paths, &s.account).context("ACCOUNT_UNAVAILABLE")?;
    let observation = claude::observe(&home, &s.conversation_id, s.attachment.as_ref());
    let prior = r;
    r = new_rec(&s, &run.id);
    r.attempt = prior.attempt;
    r.warnings = prior.warnings;
    r.home = Some(home.clone());
    r.from = Some(from.clone());
    r.to = Some(to.clone());
    r.accept_risks = o.accept_risks;
    r.prompt = o.prompt.clone();
    match observation.availability {
        "stopped" => {
            // Item 5: lifecycle admission is held for the whole re-key, so a
            // resume cannot start between the quiet check and the rename.
            let _guard = crate::lifecycle::admit(paths, &s.conversation_id, Some(&s.id))?;
            let mut ctx = Ctx::new(paths, &run.backup, run.allow_full_copy);
            r.state = "rekeying".into();
            r.phase = "admitted".into();
            save_session(paths, &mut r)?;
            if let Err(e) = rekey_stopped(&mut ctx, &mut r, &s) {
                abandon_rekey(paths, &mut r, &e)?;
                bail!("{e:#}");
            }
            Ok(json!({"session":row(&r)}))
        }
        "running" => admit_running(paths, &run, s, r, &home).await,
        "stale" => bail!("SESSION_STALE: the Claude transcript is missing"),
        _ => bail!("SESSION_STATE_UNVERIFIED: the Claude session state could not be observed"),
    }
}

/// B3(c): while files of an earlier attempt are still in the new key (or the
/// catalog still names it), a new attempt would replace the record and lose
/// the list of moves: refuse until the session is rolled back.
pub(crate) fn retry_allowed(r: &SessionRec) -> Result<()> {
    ensure!(
        matches!(r.state.as_str(), "rekeyed" | "verified")
            || (r.moved.is_empty() && r.revision_after.is_none()),
        "ROOT_REKEY_CONFLICT: an earlier attempt left {} name(s) moved; run `root rollback --session {}` first",
        r.moved.len(),
        r.session_id
    );
    Ok(())
}

fn row(r: &SessionRec) -> Value {
    json!({"id":r.session_id,"name":r.name,"space":r.space,"provider":r.provider,"state":r.state,
        "reason":r.reason,"phase":r.phase,"detail":r.detail,"from":r.from,"to":r.to,"key_from":r.key_from,"key_to":r.key_to,
        "retained":r.retained.iter().map(|x| json!({"name":x.name,"copy":x.copy,"bytes":x.bytes,"sha256":x.sha256})).collect::<Vec<_>>(),
        "retained_dir":r.retained_dir,"verified_at":r.verified_at,"verification":r.verification,"codex_job":r.codex_job})
}
fn worker_lock(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(session_file(paths, id)?.with_extension("worker.lock"))
}
pub(crate) fn worker_gone_pub(paths: &Paths, r: &SessionRec) -> bool {
    worker_gone(paths, r)
}
fn worker_gone(paths: &Paths, r: &SessionRec) -> bool {
    worker_lock(paths, &r.session_id)
        .and_then(|p| crate::background::worker_stopped(&p))
        .unwrap_or(false)
}
fn spawn(paths: &Paths, r: &SessionRec) -> Result<()> {
    let log = session_file(paths, &r.session_id)?.with_extension("worker.log");
    crate::background::self_worker(paths, &log, &["root-rekey-worker", &r.session_id])?
        .spawn_reaped()
        .context("ROOT_REKEY_WORKER_FAILED: the worker did not start")?;
    Ok(())
}

async fn admit_running(
    paths: &Paths,
    run: &Run,
    s: Session,
    mut r: SessionRec,
    home: &Path,
) -> Result<Value> {
    let live = claude::live_sessions(home)?;
    let holders: Vec<_> = live
        .iter()
        .filter(|l| r.conversation_ids.contains(&l.session_id))
        .collect();
    ensure!(
        holders.len() == 1,
        "AMBIGUOUS_SESSION: the process holding this conversation is not unique"
    );
    let l = holders[0];
    ensure!(
        l.kind.as_deref().is_none_or(|k| k == "interactive")
            && l.job_id.is_none()
            && l.parked_job_id.is_none(),
        "ROOT_REKEY_UNSUPPORTED: background and parked Claude sessions are not re-keyed; it stays on its old path"
    );
    ensure!(
        l.status.as_deref() != Some("busy") || r.accept_risks,
        "RISK_CONFIRMATION_REQUIRED: the session is busy; stopping it interrupts the current turn. Repeat with --accept-risks to proceed"
    );
    let attached = s
        .attachment
        .as_ref()
        .filter(|a| a.pid == l.pid && a.process_start == l.process_start)
        .context(
            "ROOT_REKEY_UNSUPPORTED: the running process is not in a pane Sessionsation recorded",
        )?;
    ensure!(
        attached.target.starts_with("herdr/"),
        "ROOT_REKEY_UNSUPPORTED: only sessions in a HERDR pane are re-keyed while running; it stays on its old path"
    );
    let to = r.to.clone().unwrap_or_default();
    ensure!(
        Path::new(&to).is_dir(),
        "ROOT_REKEY_UNSUPPORTED: {to} is not a directory"
    );
    ensure!(
        keys::key(&to).len() <= keys::KEY_MAX,
        "ROOT_REKEY_UNSUPPORTED: the new project key would be too long"
    );
    // Preflight before stopping anything: duplicates, collisions, memory.
    let projects = home.join("projects");
    let found = keys::locate(&projects, &r.conversation_ids)?;
    let coll = keys::collisions(&projects, &found, &keys::key(&to));
    ensure!(
        coll.is_empty(),
        "ROOT_REKEY_CONFLICT: {} already exist(s) in the target key; nothing was changed",
        coll.join(", ")
    );
    let from = r.from.clone().unwrap_or_default();
    let (km, kt) = (keys::key(&from), keys::key(&to));
    if km != kt {
        let mut base = read_bases(paths)
            .0
            .get(&keys::pair(&km, &kt))
            .cloned()
            .unwrap_or_default();
        let pre = keys::sync_memory(
            &projects.join(&km).join("memory"),
            &projects.join(&kt).join("memory"),
            &mut base,
            Path::new(""), // dry run: writes nothing
            &kt,
            true,
        )?;
        ensure!(
            pre.clean(),
            "MEMORY_CONFLICT: {} differ(s) between the old and new key's memory; resolve them first (nothing was stopped)",
            pre.conflicts.join(", ")
        );
    }
    let args = crate::claude_switch::read_cmdline(l.pid);
    r.permissions = Some(
        if args.iter().any(|a| a == claude::YOLO_FLAG) {
            "yolo"
        } else {
            "default"
        }
        .into(),
    );
    // S10 (review 2): a Claude delivery in flight reads the transcript by its
    // current path; wait (bounded) for it, else refuse with a retryable code.
    let deadline = Instant::now() + crate::timing::budget(Duration::from_secs(30));
    loop {
        let pending = crate::messaging::claude_steer::in_flight(paths, &s.conversation_id);
        if pending.is_empty() {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "SESSION_DELIVERY_IN_PROGRESS: message(s) {} to this session are still being delivered; retry when they finish (nothing was stopped)",
            pending.join(", ")
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    r.pane = Some(attached.target.clone());
    r.process = Some((l.pid, l.process_start));
    r.state = "rekeying".into();
    r.run = run.id.clone();
    daemon::ensure_started(paths).await?;
    let _guard = crate::lifecycle::admit(paths, &s.conversation_id, Some(&s.id))?;
    phase(
        paths,
        &mut r,
        "admitted",
        "Admitted; waiting for the worker.",
    )?;
    spawn(paths, &r)?;
    Ok(json!({"session":row(&r),"worker":"started"}))
}
use crate::daemon;

/// `root-rekey-worker <catalog id>` (detached).
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = lock_file(&worker_lock(paths, id)?)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut r = read_session(paths, id).context("OPERATION_NOT_FOUND")?;
    if r.phase == "complete" || r.phase == "failed" || r.phase == "uncertain" {
        return Ok(());
    }
    let run = latest_run(paths).context("ROOT_MIGRATION_MISMATCH")?;
    if r.rollback {
        if let Err(e) = drive_rollback(paths, &mut r).await {
            r.uncertain_from = Some(r.phase.clone());
            r.state = "uncertain".into();
            r.reason = Some(format!("{e:#}"));
            let _ = phase(
                paths,
                &mut r,
                "uncertain",
                &format!("{e:#}. Rerun `root rollback --session S`."),
            );
        }
    } else if let Err(e) = drive(paths, &run, &mut r).await {
        fail(paths, &mut r, &e).await;
    }
    crate::api::refresh_active_snapshot(paths).await;
    Ok(())
}

async fn drive(paths: &Paths, run: &Run, r: &mut SessionRec) -> Result<()> {
    let s = catalog::find(paths, &r.session_id)?;
    let home = home_of(paths, r)?;
    if order(&r.phase) < order("stopped") {
        let _guard = crate::lifecycle::admit(paths, &r.conversation_id, Some(&r.id))?;
        stop(paths, r, &home).await?;
    }
    if order(&r.phase) < order("switched") {
        let mut ctx = Ctx::new(paths, &run.backup, run.allow_full_copy);
        rekey_until(&mut ctx, r, &s, false)?;
    }
    if order(&r.phase) < order("resuming") {
        let to = r.to.clone().unwrap_or_default();
        resume(paths, r, &to).await?;
    }
    if order(&r.phase) < order("complete") {
        verify_resumed(paths, r, &home).await?;
    }
    let _ = run;
    Ok(())
}

async fn stop(paths: &Paths, r: &mut SessionRec, home: &Path) -> Result<()> {
    phase(
        paths,
        r,
        "stopping",
        "Stopping the verified Claude process (SIGTERM, never a forced kill).",
    )?;
    stop_process(r, home).await?;
    // B3 (review 2): from here on every failure resumes the session.
    r.stopped_by_worker = r.process.is_some();
    phase(paths, r, "stopped", "The Claude process has exited.")
}
/// pidfd-bound SIGTERM of the recorded process; never a forced kill.
async fn stop_process(r: &SessionRec, home: &Path) -> Result<()> {
    if let Some((pid, start)) = r.process {
        let live = claude::live_sessions(home)?;
        match live
            .iter()
            .find(|l| r.conversation_ids.contains(&l.session_id))
        {
            None => ensure!(
                crate::proc::identity::gone(pid, start),
                "SESSION_STATE_UNVERIFIED: the process is no longer registered but still runs; nothing was signalled"
            ),
            Some(l) if l.pid == pid && l.process_start == start => {
                if let Some(pane) = &r.pane {
                    crate::terminal::retain(pane, true)?;
                }
                crate::proc::identity::terminate(pid, start)?;
                let deadline = Instant::now() + Duration::from_secs(30);
                while !crate::proc::identity::gone(pid, start) {
                    ensure!(
                        Instant::now() < deadline,
                        "STOP_TIMEOUT: Claude process {pid} did not exit within 30s of SIGTERM; it was not killed"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            Some(_) => bail!(
                "SESSION_CHANGED: a different process now holds the conversation; nothing was signalled"
            ),
        }
    }
    Ok(())
}

fn pane_command(paths: &Paths, r: &SessionRec, generation: &str) -> Result<String> {
    use crate::terminal::shell_quote;
    let pane = r.pane.as_deref().context("SESSION_PANE_UNKNOWN")?;
    let mut args = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "--data-dir".into(),
        paths.root.to_string_lossy().into_owned(),
    ];
    if let Some(mode) = &r.permissions {
        args.extend(["--permissions".into(), mode.clone()]);
    }
    args.extend([
        "resume".into(),
        r.session_id.clone(),
        "--expected-generation".into(),
        generation.into(),
    ]);
    let quoted = args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let (session, _) = crate::terminal::target(pane)?;
    Ok(format!(
        "env -u TMUX -u TMUX_PANE SESSIONSATION_HERDR_SESSION={} {quoted}",
        shell_quote(session)
    ))
}

/// Type the resume once into the recorded pane, in `cwd`. Never replayed.
async fn resume(paths: &Paths, r: &mut SessionRec, cwd: &str) -> Result<()> {
    phase(
        paths,
        r,
        "resume_pending",
        "Waiting for the pane's shell before resuming.",
    )?;
    let pane = r.pane.clone().context("SESSION_PANE_UNKNOWN")?;
    let socket = crate::external::hosting::socket_for(paths, &pane)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if crate::terminal::state_at(&pane, socket.as_deref()).is_ok_and(|(idle, _)| idle) {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "PANE_NOT_AVAILABLE: the pane did not return to an idle shell; the resume was not dispatched"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut current = catalog::find(paths, &r.session_id)?;
    // The supervisor clears the attachment of a process that ended; do it first
    // (the stopped process is gone), so the generation typed below stays current.
    if current
        .attachment
        .as_ref()
        .is_some_and(|a| crate::proc::identity::gone(a.pid, a.process_start))
    {
        current = catalog::invalidate(paths, &current.id, &current.generation())
            .or_else(|_| catalog::find(paths, &r.session_id))?;
    }
    let command = pane_command(paths, r, &current.generation())?;
    r.resume_dispatched_at = Some(now());
    phase(
        paths,
        r,
        "resuming",
        "Resume dispatched to the pane. It is never replayed.",
    )?;
    crate::terminal::relaunch_at(&pane, cwd, &command, socket.as_deref())?;
    Ok(())
}

/// Outcome of one look at the resumed process (V1-V5).
enum Check {
    Pending,
    Verified(Value),
    /// A positive failure (new conversation id, wrong cwd, digest mismatch).
    Failed(String, Value),
}
fn check(paths: &Paths, r: &SessionRec, home: &Path, expect_dir: &str, expect_key: &str) -> Check {
    let Ok(s) = catalog::find(paths, &r.session_id) else {
        return Check::Pending;
    };
    let Some(a) = s.attachment.as_ref() else {
        return Check::Pending;
    };
    if Some(&a.target) != r.pane.as_ref() {
        return Check::Pending;
    }
    let Ok(live) = claude::live_sessions(home) else {
        return Check::Pending;
    };
    let Some(l) = live.iter().find(|l| l.pid == a.pid) else {
        return Check::Pending;
    };
    let mut v = json!({"pid":l.pid,"session_id":l.session_id,"cwd":l.cwd});
    // V1: same conversation.
    if !r.conversation_ids.contains(&l.session_id) {
        return Check::Failed(
            format!("V1: the pane runs a new conversation {}", l.session_id),
            v,
        );
    }
    // V2: cwd literally the expected directory (registry and /proc).
    let proc_cwd = fs::read_link(format!("/proc/{}/cwd", l.pid))
        .map(|p| p.to_string_lossy().into_owned())
        .ok();
    v["proc_cwd"] = json!(proc_cwd);
    if l.cwd.as_deref() != Some(expect_dir) || proc_cwd.as_deref().is_some_and(|c| c != expect_dir)
    {
        return Check::Failed(
            format!(
                "V2: the resumed process runs in {:?}, not {expect_dir}",
                l.cwd
            ),
            v,
        );
    }
    // V3: exactly one transcript; the retained bytes are its prefix.
    let projects = home.join("projects");
    let found = match keys::locate(&projects, std::slice::from_ref(&l.session_id)) {
        Ok(f) => f,
        Err(e) => return Check::Failed(format!("V3: {e:#}"), v),
    };
    let Some(f) = found.first() else {
        return Check::Pending;
    };
    if f.key != expect_key {
        v["relocated_by_provider"] = json!(f.key);
    }
    if let Some(ret) = r
        .retained
        .iter()
        .find(|x| !x.dir && x.name == format!("{}.jsonl", l.session_id))
    {
        match keys::sha256_prefix(&f.transcript, ret.bytes) {
            Ok(Some(d)) if d == ret.sha256 => {}
            Ok(Some(_)) => {
                return Check::Failed(
                    "V3: the live transcript does not start with the pre-move bytes".into(),
                    v,
                );
            }
            _ => return Check::Pending,
        }
    }
    v["transcript_key"] = json!(f.key);
    // V5: memory verified (every old file present in the new key, or newer there).
    if let (Some(km), Some(kt)) = (&r.memory_key, &r.key_to)
        && km != kt
        && expect_key == kt
    {
        let mut base = read_bases(paths)
            .0
            .get(&keys::pair(km, kt))
            .cloned()
            .unwrap_or_default();
        let checked = keys::sync_memory(
            &projects.join(km).join("memory"),
            &projects.join(kt).join("memory"),
            &mut base,
            Path::new(""), // dry run: writes nothing
            kt,
            true,
        );
        if let Err(e) = &checked {
            v["memory_error"] = json!(format!("{e:#}"));
        }
        if let Ok(m) = checked {
            v["memory"] = json!({"missing":m.copied,"conflicts":m.conflicts});
            if !m.copied.is_empty() || !m.fast_forwarded.is_empty() {
                // Written on the old side since the sync: merge once more.
                let mut bases = read_bases(paths);
                let base = bases.0.entry(keys::pair(km, kt)).or_default();
                let _ = keys::sync_memory(
                    &projects.join(km).join("memory"),
                    &projects.join(kt).join("memory"),
                    base,
                    &memory_backup(paths, r),
                    kt,
                    false,
                );
                let _ = save_bases(paths, &bases);
            }
        }
    }
    v["pid_start"] = json!(l.process_start);
    Check::Verified(v)
}

fn budget(r: &SessionRec) -> Duration {
    let bytes: u64 = r.retained.iter().filter(|x| !x.dir).map(|x| x.bytes).sum();
    crate::timing::budget(Duration::from_secs(60 + bytes / (10 << 20)))
}

async fn verify_resumed(paths: &Paths, r: &mut SessionRec, home: &Path) -> Result<()> {
    // The phase stays `resuming` (which lifecycle admission lets the typed
    // `resume` through) while V1-V5 are checked.
    r.detail = "Verifying the resumed conversation (V1-V5).".into();
    save_session(paths, r)?;
    let to = r.to.clone().unwrap_or_default();
    let kt = keys::key(&to);
    let deadline = Instant::now() + budget(r);
    loop {
        match check(paths, r, home, &to, &kt) {
            Check::Verified(v) => {
                r.verification = v.clone();
                r.resumed_pid = v["pid"]
                    .as_u64()
                    .zip(v["pid_start"].as_u64())
                    .map(|(p, s)| (p as u32, s));
                // S4 (review 2): V5 problems are reported, never reverted (the
                // session runs on its new path).
                let conflicts = v["memory"]["conflicts"]
                    .as_array()
                    .is_some_and(|c| !c.is_empty());
                if let Some(e) = v["memory_error"].as_str() {
                    r.warnings
                        .push(format!("V5: memory could not be verified: {e}"));
                }
                if conflicts {
                    r.state = "verify_failed".into();
                    r.reason = Some(format!(
                        "V5: memory conflict(s) between the old and new key: {}",
                        v["memory"]["conflicts"]
                    ));
                } else {
                    r.state = "verified".into();
                }
                r.verified_at = Some(now());
                phase(
                    paths,
                    r,
                    "complete",
                    "Resumed: same conversation, same pane, new directory, pre-move bytes are a prefix.",
                )?;
                deliver_prompt(paths, r).await;
                return Ok(());
            }
            Check::Failed(why, v) => {
                r.verification = v;
                return revert(paths, r, home, &why).await;
            }
            Check::Pending if Instant::now() >= deadline => {
                // R10: a slow load is uncertain, never a revert.
                r.uncertain_from = Some("verifying".into());
                r.state = "uncertain".into();
                return phase(
                    paths,
                    r,
                    "uncertain",
                    "The resume was dispatched but not confirmed in time. Inspect the pane, then `root migrate --session S` to re-check or `root rollback --session S`.",
                );
            }
            Check::Pending => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// After a positive verification failure: stop only the process this worker
/// started, move the files back, restore the directory, resume on the old path.
async fn revert(paths: &Paths, r: &mut SessionRec, home: &Path, why: &str) -> Result<()> {
    r.warnings.push(why.to_owned());
    phase(
        paths,
        r,
        "reverting",
        &format!("{why}; moving the conversation back."),
    )?;
    if let Ok(s) = catalog::find(paths, &r.session_id)
        && let Some(a) = s.attachment.as_ref()
        && Some(&a.target) == r.pane.as_ref()
        && claude::live_sessions(home).is_ok_and(|l| l.iter().any(|l| l.pid == a.pid))
    {
        crate::proc::identity::terminate(a.pid, a.process_start)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while !crate::proc::identity::gone(a.pid, a.process_start) {
            ensure!(
                Instant::now() < deadline,
                "STOP_TIMEOUT: the new process did not exit"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    // A fresh-conversation file the failed resume created stays (reported, never deleted).
    keys::move_back(&r.moved)?;
    r.moved.clear();
    if let (Some(to), Some(before), Some(after)) =
        (r.to.clone(), r.revision_before, r.revision_after)
    {
        catalog::restore_directory(
            paths,
            &r.session_id,
            &r.conversation_id,
            &to,
            r.from.as_deref(),
            before,
            after,
        )?;
    }
    let from = r.from.clone().unwrap_or_default();
    resume(paths, r, &from).await?;
    let km = keys::key(&from);
    let deadline = Instant::now() + budget(r);
    loop {
        match check(paths, r, home, &from, &km) {
            Check::Verified(_) => {
                r.state = "left_behind".into();
                r.reason = Some(why.into());
                return phase(
                    paths,
                    r,
                    "complete",
                    "Resumed on the old path; left behind.",
                );
            }
            Check::Pending if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            _ => {
                r.state = "uncertain".into();
                r.uncertain_from = Some("complete".into());
                return phase(
                    paths,
                    r,
                    "uncertain",
                    "Moved back, but the resume on the old path was not confirmed; the retained copy is untouched.",
                );
            }
        }
    }
}

async fn fail(paths: &Paths, r: &mut SessionRec, e: &anyhow::Error) {
    // After a failed move the names are back (`moved` empty) and the phase was
    // reset: `stopped_by_worker` still says the worker owes the session a resume.
    let before_move =
        r.moved.is_empty() && (order(&r.phase) < order("moved") || r.phase == "admitted");
    let stopped =
        r.stopped_by_worker || (r.process.is_some() && order(&r.phase) >= order("stopped"));
    if before_move {
        r.reason = Some(format!("{e:#}"));
        if stopped && r.pane.is_some() {
            // Nothing moved, but the process was stopped: resume on the old path.
            let from = r.from.clone().unwrap_or_default();
            if resume(paths, r, &from).await.is_ok() {
                r.state = "left_behind".into();
                let _ = phase(
                    paths,
                    r,
                    "complete",
                    &format!("{e:#}. Nothing moved; resumed on the old path."),
                );
                return;
            }
        }
        r.state = if stopped { "uncertain" } else { "failed" }.into();
        let _ = phase(
            paths,
            r,
            if stopped { "uncertain" } else { "failed" },
            &format!("{e:#}. Nothing moved."),
        );
        return;
    }
    r.uncertain_from = Some(r.phase.clone());
    r.state = "uncertain".into();
    r.reason = Some(format!("{e:#}"));
    let _ = phase(
        paths,
        r,
        "uncertain",
        &format!("{e:#}. Continue with `root migrate --session S` or `root rollback --session S`."),
    );
}

/// `--prompt`: delivered once after verification (journaled before sending; never replayed).
async fn deliver_prompt(paths: &Paths, r: &mut SessionRec) {
    let Some(text) = r.prompt.clone() else {
        return;
    };
    if r.prompt_message_id.is_some() {
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    r.prompt_message_id = Some(id.clone());
    r.prompt_state = Some("sending".into());
    let _ = save_session(paths, r);
    let generation = catalog::find(paths, &r.session_id)
        .map(|s| s.generation())
        .unwrap_or_default();
    let request = json!({"api_version":1,"message_id":id,"mode":"submit","session_id":r.session_id,
        "expected_generation":generation,"text":text,"expires_at":now() + 3600});
    r.prompt_state = Some(
        match crate::messaging::claude_steer::send(paths, request, None, None).await {
            Ok(_) => "accepted".into(),
            Err(e) => format!(
                "failed: {}",
                format!("{e:#}").split(':').next().unwrap_or("error")
            ),
        },
    );
    let _ = save_session(paths, r);
}

// -------------------------------------------------------------- Codex Tier S

async fn migrate_codex(
    paths: &Paths,
    run: &Run,
    alias: &Alias,
    s: Session,
    mut r: SessionRec,
    o: &SessionOptions,
) -> Result<Value> {
    if r.state == "relocating" {
        refresh_codex(paths, &mut r).await?;
        return Ok(json!({"session":row(&r)}));
    }
    if r.state == "rekeyed" {
        return Ok(json!({"session":row(&r),"noop":true}));
    }
    let (cwd, _) = codex_cwd(paths, &s);
    // Fail closed: an unreadable inventory never means "not running".
    let codex = codex_instances(paths)
        .await
        .map_err(|e| anyhow::anyhow!("SESSION_STATE_UNVERIFIED: probe_failed: {e}"))?;
    let live = codex
        .iter()
        .find(|i| !i.inactive && i.thread_id == s.conversation_id)
        .cloned();
    let cwd = live
        .as_ref()
        .map(|i| i.cwd.clone())
        .or(cwd)
        .context("SAVED_DIRECTORY_UNAVAILABLE")?;
    let to = alias
        .logical(Path::new(&cwd))
        .to_string_lossy()
        .into_owned();
    ensure!(
        // raw-path: the mapping changes the spelling or the cwd is not under an entry
        to != cwd,
        "ROOT_REKEY_UNSUPPORTED: {cwd} is not under an alias entry"
    );
    ensure!(
        Path::new(&to).is_dir(),
        "ROOT_REKEY_UNSUPPORTED: {to} is not a directory"
    );
    r = new_rec(&s, &run.id);
    r.from = Some(cwd);
    r.to = Some(to.clone());
    match live {
        None => {
            codex_rekey_stopped(paths, &mut r)?;
            r.state = "rekeyed".into();
            r.phase = "complete".into();
            save_session(paths, &mut r)?;
            Ok(json!({"session":row(&r)}))
        }
        Some(source) => {
            let rollout = source.rollout.clone();
            if let Ok((bytes, digest)) = keys::sha256_file(&rollout) {
                r.codex_rollout = Some((rollout, bytes, digest));
            }
            r.pane = Some(source.pane.clone());
            // The directory job writes the thread location; keep what was there.
            r.codex_location_before = fs::read_to_string(
                paths
                    .root
                    .join(layout::THREAD_LOCATIONS)
                    .join(format!("{}.json", r.conversation_id)),
            )
            .ok();
            daemon::ensure_started(paths).await?;
            let generation = o
                .expected_generation
                .clone()
                .unwrap_or_else(|| s.generation());
            let job = crate::external::relocation::queue_public(
                paths,
                source,
                Path::new(&to),
                false,
                (&s.id, &generation),
            )
            .await?;
            r.codex_job = Some(job.id.clone());
            r.state = "relocating".into();
            r.phase = "complete".into();
            r.detail =
                "The existing directory job restarts the thread in the new directory.".into();
            save_session(paths, &mut r)?;
            Ok(json!({"session":row(&r),"job":job.id}))
        }
    }
}

/// Settle a Codex Tier S re-key from its directory job.
pub(crate) async fn refresh_codex(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    let Some(id) = r.codex_job.clone() else {
        return Ok(());
    };
    let Some(job) = crate::external::jobs(paths)?
        .into_iter()
        .find(|j| j.id == id)
    else {
        return Ok(());
    };
    match job.phase.as_str() {
        "complete" => {
            let to = r.to.clone().unwrap_or_default();
            // The directory job wrote the thread location: record it (item 4).
            let file = paths
                .root
                .join(layout::THREAD_LOCATIONS)
                .join(format!("{}.json", r.conversation_id));
            if !r.codex_location_written {
                r.codex_location_written = true;
                r.codex_location_after = fs::read_to_string(&file).ok();
            }
            let Ok(instances) = codex_instances(paths).await else {
                return Ok(()); // undecided: re-checked by the next status
            };
            let live = instances
                .into_iter()
                .find(|i| !i.inactive && i.thread_id == r.conversation_id);
            let mut v = json!({"job":id,"job_phase":job.phase});
            let ok_pane = live
                .as_ref()
                .is_some_and(|i| Some(&i.pane) == r.pane.as_ref());
            let ok_cwd = live.as_ref().is_some_and(|i| i.cwd == to); // raw-path: V2 is literal
            let ok_prefix = r.codex_rollout.as_ref().is_none_or(|(p, bytes, d)| {
                keys::sha256_prefix(p, *bytes).ok().flatten().as_deref() == Some(d.as_str())
            });
            v["same_pane"] = json!(ok_pane);
            v["cwd"] = json!(live.as_ref().map(|i| i.cwd.clone()));
            v["rollout_prefix"] = json!(ok_prefix);
            r.verification = v;
            if ok_pane && ok_cwd && ok_prefix {
                r.state = "verified".into();
                r.verified_at = Some(now());
            } else if live.is_some() {
                r.state = "verify_failed".into();
            }
        }
        "failed" | "cancelled" | "recovered" => {
            r.state = "left_behind".into();
            r.reason = Some(format!("directory job {}: {}", job.phase, job.detail));
        }
        _ => return Ok(()),
    }
    save_session(paths, r)
}

// ---------------------------------------------------------- rollback --session

/// `root rollback --session S`.
pub async fn rollback_session(paths: &Paths, session: &str, dry_run: bool) -> Result<Value> {
    let s = catalog::resolve_target(paths, session)?;
    let mut r = read_session(paths, &s.id)
        .context("ROOT_MIGRATION_MISMATCH: this session has no re-key record")?;
    if dry_run {
        return Ok(json!({"dry_run":true,"session":row(&r),"moves_back":r.moved}));
    }
    ensure!(
        r.phase == "complete"
            || r.phase == "uncertain"
            || r.phase == "failed"
            || worker_gone(paths, &r),
        "SESSION_OPERATION_IN_PROGRESS: the re-key worker is still running"
    );
    if project_keyed(&r.provider) && r.state == "archived_rekey_at_restore" {
        rollback_stopped(paths, &mut r)?;
        return Ok(json!({"session":row(&r)}));
    }
    let home = claude_home(paths, &r.account);
    let running = home.as_ref().is_some_and(|h| {
        claude::observe(h, &s.conversation_id, s.attachment.as_ref()).availability == "running"
    });
    if project_keyed(&r.provider) && running {
        // Item 5: a detached worker with lifecycle admission stops it, moves the
        // files back and resumes it on the old path (never this CLI process).
        let home = home.context("ACCOUNT_UNAVAILABLE")?;
        let live = claude::live_sessions(&home)?;
        let l = live
            .iter()
            .find(|l| r.conversation_ids.contains(&l.session_id))
            .context("SESSION_STATE_UNVERIFIED")?;
        r.process = Some((l.pid, l.process_start));
        r.pane = s.attachment.as_ref().map(|a| a.target.clone());
        r.home = Some(home.clone());
        ensure!(
            r.pane.as_deref().is_some_and(|p| p.starts_with("herdr/")),
            "ROOT_REKEY_UNSUPPORTED: only sessions in a HERDR pane are rolled back while running"
        );
        r.rollback = true;
        daemon::ensure_started(paths).await?;
        let _guard = crate::lifecycle::admit(paths, &s.conversation_id, Some(&s.id))?;
        phase(
            paths,
            &mut r,
            "rollback_admitted",
            "Rollback admitted; waiting for the worker.",
        )?;
        spawn(paths, &r)?;
        return Ok(json!({"session":row(&r),"worker":"started"}));
    }
    // Not running: under lifecycle admission so no resume starts meanwhile.
    let _guard = crate::lifecycle::admit(paths, &s.conversation_id, Some(&s.id))?;
    rollback_stopped(paths, &mut r)?;
    Ok(json!({"session":row(&r)}))
}

/// The rollback worker: stop, move back, resume on the old path, confirm.
async fn drive_rollback(paths: &Paths, r: &mut SessionRec) -> Result<()> {
    let home = home_of(paths, r)?;
    if matches!(r.phase.as_str(), "rollback_admitted" | "rollback_stopping") {
        let _guard = crate::lifecycle::admit(paths, &r.conversation_id, Some(&r.id))?;
        phase(
            paths,
            r,
            "rollback_stopping",
            "Stopping the verified Claude process.",
        )?;
        stop_process(r, &home).await?;
        phase(
            paths,
            r,
            "rollback_stopped",
            "The Claude process has exited.",
        )?;
    }
    if r.phase == "rollback_stopped" {
        let _guard = crate::lifecycle::admit(paths, &r.conversation_id, Some(&r.id))?;
        undo_session(paths, r)?;
        let from = r.from.clone().unwrap_or_default();
        resume(paths, r, &from).await?;
    }
    let from = r.from.clone().unwrap_or_default();
    let km = keys::key(&from);
    let deadline = Instant::now() + budget(r);
    loop {
        match check(paths, r, &home, &from, &km) {
            Check::Verified(v) => {
                r.verification = v;
                r.state = "not_applicable".into();
                r.reason = Some("rolled_back".into());
                r.rollback = false;
                return phase(
                    paths,
                    r,
                    "complete",
                    "Rolled back and resumed on the old path.",
                );
            }
            Check::Pending if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            _ => {
                r.state = "uncertain".into();
                r.uncertain_from = Some("resuming".into());
                return phase(
                    paths,
                    r,
                    "uncertain",
                    "Rolled back, but the resume on the old path was not confirmed; inspect the pane.",
                );
            }
        }
    }
}
