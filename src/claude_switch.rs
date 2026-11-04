//! Claude Code account switch (contract 1.34.0): move one conversation from
//! `claude:A` to `claude:B` and resume it there, mirroring the Codex switch.
//!
//! Journaled in `operations/claude-switches/<id>.json`, driven by a detached
//! worker (`claude-switch-worker`) and never replayed automatically. Phases:
//!
//! ```text
//! admitted -> stopping -> stopped -> copying -> copied -> switching -> switched
//!   -> removing -> removed -> resume_pending -> resuming -> complete
//!      \_ failed (before the catalog changed: target copies removed, source intact)
//!      \_ uncertain (after it changed, or an unconfirmed resume) -> recover | abandoned
//! ```
//!
//! Crash safety: files are copied into a staging directory in B's home, verified
//! by digest, renamed into place, only then is the catalog switched, and only
//! then are A's copies removed (digest-verified again). `api recover` continues
//! from the journaled phase; the stop signal is pidfd-bound to the verified
//! process and the resume dispatch is never replayed (evidence only).
//!
//! Moved: the transcript `projects/<slug>/<id>.jsonl` and its sidecars
//! (`projects/<slug>/<id>/`, `file-history/<id>/`, `session-env/<id>/`,
//! `todos/<id>-agent-*.json`) for every conversation of the lineage. NOT moved:
//! per-project `projects/<slug>/memory/`, settings, credentials and any other
//! account-level state stay with the source account.
//!
//! Shared store (contract 1.39.0): when both homes link to `<root>/shared/claude`
//! the copy and remove phases are journaled no-ops (nothing copied or deleted).
use crate::{
    catalog::{self, Session},
    daemon,
    external::archiving::native as files,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{AccountRef, ProviderId, claude, native},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

const ORDER: &[&str] = &[
    "admitted",
    "stopping",
    "stopped",
    "copying",
    "copied",
    "switching",
    "switched",
    "removing",
    "removed",
    "resume_pending",
    "resuming",
    "complete",
];
pub const PHASES_TERMINAL: &[&str] = &["complete", "failed", "abandoned"];
const STOP_SECONDS: u64 = 30;
const SHELL_SECONDS: u64 = 20;
const CONFIRM_SECONDS: u64 = 30;
const STAGING: &str = ".sessionsation-switch";

fn order(phase: &str) -> usize {
    ORDER.iter().position(|p| *p == phase).unwrap_or(usize::MAX)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    relative: PathBuf,
    dir: bool,
    bytes: u64,
    digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Process {
    pid: u32,
    process_start: u64,
    status: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    schema_version: u32,
    id: String,
    kind: String,
    session: Session,
    session_id: String,
    /// Top level so lifecycle admission blocks other operations on the conversation.
    conversation_id: String,
    conversation_ids: Vec<String>,
    expected_generation: String,
    /// Both homes link to the shared Claude store: the copy and remove phases move nothing.
    #[serde(default)]
    shared: bool,
    accept_risks: bool,
    source_account: String,
    target_account: String,
    source_home: PathBuf,
    target_home: PathBuf,
    directory: Option<String>,
    /// Pane the live process ran in (resume target); None for a stopped session.
    pane: Option<String>,
    process: Option<Process>,
    /// Recorded permissions of the stopped process (`yolo` | `default`).
    permissions: Option<String>,
    /// Digests journaled once the source is quiescent.
    entries: Option<Vec<Entry>>,
    phase: String,
    detail: String,
    uncertain_from: Option<String>,
    /// not_needed | pending | dispatching | confirmed | unconfirmed
    resume: String,
    generation_after: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::CLAUDE_SWITCHES)
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
fn worker_lock_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(file(paths, id)?.with_extension("worker.lock"))
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|f| f.is_file())
}
fn read(paths: &Paths, id: &str) -> Result<Record> {
    let bytes = fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?;
    serde_json::from_slice(&bytes).context("UNREADABLE_OPERATION_RECORD")
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = daemon::now();
    private_dir(&dir(paths))?;
    atomic_write(&file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
fn phase(paths: &Paths, r: &mut Record, name: &str, detail: &str) -> Result<()> {
    r.phase = name.into();
    r.detail = detail.into();
    save(paths, r)?;
    #[cfg(test)]
    tests::crash_check(name)?;
    Ok(())
}
fn list(paths: &Paths) -> Result<Vec<Record>> {
    let mut all = vec![];
    if !dir(paths).is_dir() {
        return Ok(all);
    }
    for entry in crate::paths::journal_paths(&dir(paths))? {
        let Some(id) = entry
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if let Ok(r) = read(paths, id) {
            all.push(r);
        }
    }
    all.sort_by_key(|r| (r.created_at, r.id.clone()));
    Ok(all)
}
pub fn is_terminal(r: &Record) -> bool {
    PHASES_TERMINAL.contains(&r.phase.as_str())
}
fn worker_stopped(paths: &Paths, r: &Record) -> bool {
    worker_lock_path(paths, &r.id)
        .and_then(|p| crate::background::worker_stopped(&p))
        .unwrap_or(false)
}
fn stalled(paths: &Paths, r: &Record) -> bool {
    !is_terminal(r) && daemon::now() - r.updated_at > 10 && worker_stopped(paths, r)
}

/// Operation row for `api operations` and the `api switch` result.
fn public(paths: &Paths, r: &Record) -> Value {
    let stalled = stalled(paths, r);
    let can_recover =
        !is_terminal(r) && (r.phase == "uncertain" || stalled) && worker_stopped(paths, r);
    let moved = r.entries.as_ref().map(|e| e.len());
    json!({"id":r.id,"source_session_id":r.session_id,"session_id":r.session_id,"kind":"account","provider":"claude",
    "phase":r.phase,"detail":r.detail,"uncertain":r.phase == "uncertain","terminal":is_terminal(r),"stalled":stalled,
    "confirmation_seq":0,"source_account_id":format!("claude:{}",r.source_account),
    "target_account_id":format!("claude:{}",r.target_account),"provider_conversation_id":r.conversation_id,
    "created_at":r.created_at,"updated_at":r.updated_at,"can_cancel":false,"can_recover":can_recover,"can_respond":false,
    "non_steering":true,"replays":false,
    "resume":{"state":r.resume,"pane":r.pane,"permissions":r.permissions},
    "generation":r.generation_after,
    "receipt":{"shared_state":r.shared,"files_moved":if r.shared {Some(0)} else {moved},"paths":r.entries.as_ref().map(|e| e.iter().map(|x| x.relative.to_string_lossy().into_owned()).collect::<Vec<_>>()),
        "account_memory":{"moved":false,"shared":r.shared,"stays_with":format!("claude:{}",r.source_account),
            "detail":if r.shared {"Per-project memory/ lives in the shared store and is the same on both accounts; settings and credentials stay with the source account."} else {"Per-project memory/, settings and credentials are account state and stay with the source account."}}},
    "next":match r.phase.as_str() {
        "uncertain" => "api recover <operation> to continue from the journaled phase (a resume is never replayed), or api abandon-operation <operation> --confirm <operation>",
        "complete" | "failed" | "abandoned" => "none",
        _ if stalled => "worker stopped: api recover <operation>",
        _ => "in progress",
    }})
}
pub fn operations(paths: &Paths) -> Result<Vec<Value>> {
    Ok(list(paths)?.iter().map(|r| public(paths, r)).collect())
}

// ------------------------------------------------------------------ admission

fn account_ref(to: &str) -> Result<String> {
    let account = AccountRef::parse(to)?;
    ensure!(
        account.provider == ProviderId::Claude,
        "ACCOUNT_PROVIDER_REQUIRED: a Claude session switches only to another claude:<name> account"
    );
    Ok(account.name)
}
fn read_cmdline(pid: u32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}
fn gone(pid: u32, start: u64) -> bool {
    claude::proc_start(pid).is_none_or(|(s, st)| st != start || matches!(s, 'Z' | 'X'))
}

pub async fn start(
    paths: &Paths,
    session: &str,
    expected_generation: &str,
    to: &str,
    accept_risks: bool,
) -> Result<Value> {
    let target = account_ref(to)?;
    let s = catalog::resolve(paths, session, expected_generation)?;
    ensure!(
        s.provider == ProviderId::Claude.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    ensure!(s.account != target, "SWITCH_SAME_ACCOUNT");
    let source = native::account(paths, ProviderId::Claude, &s.account)?;
    let dest = native::account(paths, ProviderId::Claude, &target)
        .map_err(|_| anyhow::anyhow!("ACCOUNT_UNAVAILABLE: claude:{target} is not enrolled"))?;
    for name in [&s.account, &target] {
        crate::accounts::ensure_available(paths, ProviderId::Claude, name)?;
        ensure!(
            !native::deleted(paths, ProviderId::Claude, name).unwrap_or(false),
            "ACCOUNT_UNAVAILABLE: claude:{name} was deleted"
        );
    }
    ensure!(
        source.enabled && dest.enabled && source.home.is_dir() && dest.home.is_dir(),
        "ACCOUNT_UNAVAILABLE"
    );
    // Ensures both homes are linked when the shared store is on. Both linked: the
    // conversation already is in both accounts, so nothing is copied or deleted.
    let shared = crate::shared_state::switch_shared(paths, &source.home, &dest.home)?;
    daemon::ensure_started(paths).await?;
    let state = daemon::call(paths, "status", json!({})).await?;
    for name in [&s.account, &target] {
        ensure!(
            state["accounts"]
                .as_array()
                .is_some_and(|aa| aa.iter().any(|a| {
                    a["name"] == name.as_str()
                        && a["provider"] == "claude"
                        && a["enabled"] == true
                        && a["auth_required"] == false
                })),
            "ACCOUNT_UNAVAILABLE: claude:{name} is disabled or needs login"
        );
    }
    ensure!(
        !claude::hosted::live(paths, &s.conversation_id),
        "SESSION_HOSTED: a Sessionsation-hosted (stream-json) Claude session cannot be switched; stop it first"
    );
    crate::messaging::headless::ensure_idle(paths, &s.conversation_id)?;
    let mut ids = s.previous_conversation_ids.clone();
    ids.push(s.conversation_id.clone());
    let observation = claude::observe(&source.home, &s.conversation_id, s.attachment.as_ref());
    let mut process = None;
    let mut pane = None;
    let mut permissions = None;
    match observation.availability {
        "stopped" => {}
        "running" => {
            let live = claude::live_sessions(&source.home)?;
            let holders: Vec<_> = live
                .iter()
                .filter(|l| ids.contains(&l.session_id))
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
                "SESSION_NOT_SWITCHABLE: background and parked Claude sessions cannot be switched"
            );
            ensure!(
                l.status.as_deref() != Some("busy") || accept_risks,
                "RISK_CONFIRMATION_REQUIRED: the session is busy; stopping it interrupts the current turn. Repeat with --accept-risks to proceed"
            );
            let attached = s
                .attachment
                .as_ref()
                .filter(|a| a.pid == l.pid && a.process_start == l.process_start)
                .context("SESSION_PANE_UNKNOWN: the running process is not in a pane Sessionsation recorded, so it cannot be resumed after the switch; end it first")?;
            pane = Some(attached.target.clone());
            let args = read_cmdline(l.pid);
            permissions = Some(
                if args.iter().any(|a| a == claude::YOLO_FLAG) {
                    "yolo"
                } else {
                    "default"
                }
                .to_owned(),
            );
            process = Some(Process {
                pid: l.pid,
                process_start: l.process_start,
                status: l.status.clone(),
            });
        }
        "stale" => bail!("SESSION_STALE: the Claude transcript is missing from its account home"),
        _ => bail!("SESSION_STATE_UNVERIFIED: the Claude session state could not be observed"),
    }
    let directory = s.directory.clone().or(observation.directory.clone());
    if process.is_some() {
        let d = directory
            .as_deref()
            .context("SAVED_DIRECTORY_UNAVAILABLE")?;
        ensure!(
            Path::new(d).is_absolute() && Path::new(d).is_dir(),
            "SAVED_DIRECTORY_UNAVAILABLE"
        );
    }
    ensure!(
        claude::find_transcript(&source.home, &s.conversation_id).is_some(),
        "HISTORY_NOT_FOUND"
    );
    ensure!(
        shared || files::collect(ProviderId::Claude, &dest.home, &ids)?.is_empty(),
        "TARGET_CONFLICT: claude:{target} already holds a copy of this conversation; nothing was moved"
    );
    let id = uuid::Uuid::new_v4().to_string();
    private_dir(&dir(paths))?;
    let admission = lock_file(&dir(paths).join("admission.lock"))?;
    admission.lock_exclusive()?;
    ensure!(
        !list(paths)?
            .iter()
            .any(|o| o.session_id == s.id && !is_terminal(o)),
        "OPERATION_BUSY"
    );
    let _guard = crate::lifecycle::admit(paths, &s.conversation_id, Some(&id))?;
    crate::lifecycle::ensure_no_handoff(paths, &s.conversation_id, Some(&id))?;
    let mut r = Record {
        schema_version: 1,
        id: id.clone(),
        kind: "account".into(),
        session_id: s.id.clone(),
        conversation_id: s.conversation_id.clone(),
        conversation_ids: ids,
        expected_generation: expected_generation.into(),
        shared,
        accept_risks,
        source_account: s.account.clone(),
        target_account: target,
        source_home: source.home,
        target_home: dest.home,
        directory,
        pane,
        process,
        permissions,
        entries: None,
        phase: "admitted".into(),
        detail: "Admitted; waiting for the worker.".into(),
        uncertain_from: None,
        resume: "pending".into(),
        generation_after: None,
        created_at: daemon::now(),
        updated_at: daemon::now(),
        session: s,
    };
    save(paths, &mut r)?;
    let child = crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["claude-switch-worker", &r.id])
    .spawn();
    if let Err(e) = child {
        phase(
            paths,
            &mut r,
            "failed",
            &format!("The worker did not start: {e}. Nothing was changed."),
        )?;
    }
    Ok(public(paths, &r))
}

// --------------------------------------------------------------------- worker

pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = lock_file(&worker_lock_path(paths, id)?)?;
    lock.lock_exclusive()?;
    let mut r = read(paths, id)?;
    if r.phase != "admitted" {
        return Ok(());
    }
    run(paths, &mut r).await;
    Ok(())
}

async fn run(paths: &Paths, r: &mut Record) {
    if let Err(error) = drive(paths, r).await {
        fail(paths, r, &error);
    }
}

fn fail(paths: &Paths, r: &mut Record, error: &anyhow::Error) {
    let committed =
        catalog::find(paths, &r.session_id).is_ok_and(|s| s.account == r.target_account);
    let before = order(&r.phase) <= order("switching") && !committed;
    let stopped = if r.process.is_some() && order(&r.phase) > order("stopping") {
        " The Claude process was already stopped; resume the session on its original account."
    } else {
        ""
    };
    let _ = if before {
        rollback(r);
        phase(
            paths,
            r,
            "failed",
            &format!(
                "{error:#}. Nothing moved: the conversation is intact on claude:{}.{stopped}",
                r.source_account
            ),
        )
    } else {
        r.uncertain_from = Some(r.phase.clone());
        phase(
            paths,
            r,
            "uncertain",
            &format!(
                "{error:#}. The catalog already points at claude:{}; use api recover to continue from '{}' or api abandon-operation.",
                r.target_account, r.phase
            ),
        )
    };
}

fn acquire(paths: &Paths, r: &Record) -> Result<Vec<fs::File>> {
    let mut held = vec![];
    held.push(crate::lifecycle::admit(
        paths,
        &r.conversation_id,
        Some(&r.id),
    )?);
    crate::lifecycle::ensure_no_handoff(paths, &r.conversation_id, Some(&r.id))?;
    held.push(crate::external::conversation_lock(
        paths,
        &r.conversation_id,
    )?);
    let mut names = [r.source_account.as_str(), r.target_account.as_str()];
    names.sort();
    for name in names {
        let mut attempts = 0;
        loop {
            match crate::accounts::operation_lock(paths, ProviderId::Claude, name) {
                Ok(lock) => {
                    held.push(lock);
                    break;
                }
                Err(e) if attempts >= 50 => return Err(e),
                Err(_) => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    Ok(held)
}

async fn drive(paths: &Paths, r: &mut Record) -> Result<()> {
    let p = order(&r.phase);
    if p <= order("removing") {
        let _locks = acquire(paths, r)?;
        if p <= order("stopping") {
            stop(paths, r).await?;
        }
        if p <= order("copying") {
            copy(paths, r)?;
        }
        if p <= order("switching") {
            switch(paths, r)?;
        }
        if p <= order("removing") {
            remove(paths, r)?;
        }
    }
    resume(paths, r).await
}

// ------------------------------------------------------------------------ stop

fn terminate(pid: u32, start: u64) -> Result<()> {
    // A pidfd names this process object even if the pid number is reused.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        ensure!(
            e.raw_os_error() == Some(libc::ESRCH),
            "STOP_UNAVAILABLE: could not open a stable handle on process {pid}: {e}"
        );
        return Ok(());
    }
    let fd = fd as libc::c_int;
    if gone(pid, start) {
        unsafe { libc::close(fd) };
        return Ok(());
    }
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd,
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    let error = std::io::Error::last_os_error();
    unsafe { libc::close(fd) };
    ensure!(
        sent == 0 || error.raw_os_error() == Some(libc::ESRCH),
        "STOP_FAILED: could not signal process {pid}: {error}"
    );
    Ok(())
}

async fn stop(paths: &Paths, r: &mut Record) -> Result<()> {
    phase(
        paths,
        r,
        "stopping",
        "Stopping the verified Claude process (SIGTERM, never a forced kill).",
    )?;
    if let Some(p) = r.process.clone() {
        let live = claude::live_sessions(&r.source_home)?;
        match live
            .iter()
            .find(|l| r.conversation_ids.contains(&l.session_id))
        {
            None => ensure!(
                gone(p.pid, p.process_start),
                "SESSION_STATE_UNVERIFIED: the process is no longer registered but still runs; nothing was signalled"
            ),
            Some(l) if l.pid == p.pid && l.process_start == p.process_start => {
                if let Some(pane) = &r.pane {
                    // tmux would close the pane with its process; HERDR keeps its shell.
                    crate::terminal::retain(pane, true)?;
                }
                terminate(p.pid, p.process_start)?;
                let deadline = std::time::Instant::now() + Duration::from_secs(STOP_SECONDS);
                while !gone(p.pid, p.process_start) {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "STOP_TIMEOUT: Claude process {} did not exit within {STOP_SECONDS}s of SIGTERM; it was not killed",
                        p.pid
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            Some(_) => bail!(
                "SESSION_CHANGED: a different process now holds the conversation; nothing was signalled"
            ),
        }
    }
    phase(paths, r, "stopped", "The Claude process has exited.")
}

// ------------------------------------------------------------------------ copy

fn plan(r: &Record) -> Result<Vec<Entry>> {
    let found = files::collect(ProviderId::Claude, &r.source_home, &r.conversation_ids)?;
    let mut entries = vec![];
    for p in found {
        files::not_open(&p)?;
        let (bytes, digest) = files::tree_digest(&p)?;
        entries.push(Entry {
            relative: p.strip_prefix(&r.source_home)?.to_path_buf(),
            dir: fs::symlink_metadata(&p)?.is_dir(),
            bytes,
            digest,
        });
    }
    ensure!(
        entries
            .iter()
            .any(|e| e.relative.extension().is_some_and(|x| x == "jsonl")),
        "HISTORY_NOT_FOUND"
    );
    Ok(entries)
}

/// Preserve modification times so "last accessed" ordering survives the copy.
fn copy_times(src: &Path, dst: &Path) {
    let Ok(meta) = fs::symlink_metadata(src) else {
        return;
    };
    if meta.is_dir() {
        if let Ok(children) = fs::read_dir(src) {
            for c in children.flatten() {
                copy_times(&c.path(), &dst.join(c.file_name()));
            }
        }
    } else if let (Ok(modified), Ok(f)) = (
        meta.modified(),
        fs::OpenOptions::new().write(true).open(dst),
    ) {
        let _ = f.set_modified(modified);
    }
}

fn entries(r: &Record) -> Result<&Vec<Entry>> {
    r.entries.as_ref().context("MISSING_FILE_PLAN")
}

fn copy(paths: &Paths, r: &mut Record) -> Result<()> {
    if r.shared {
        phase(
            paths,
            r,
            "copying",
            "Shared Claude state: both accounts link to the same store, so nothing is copied.",
        )?;
        return phase(
            paths,
            r,
            "copied",
            "Nothing to copy: the conversation is already in both account homes through the shared store.",
        );
    }
    if r.entries.is_none() {
        r.entries = Some(plan(r)?);
    }
    phase(
        paths,
        r,
        "copying",
        "Copying the conversation's files into the target account home and verifying them.",
    )?;
    let stage_root = r.target_home.join(STAGING).join(&r.id);
    for (i, e) in entries(r)?.iter().enumerate() {
        let src = r.source_home.join(&e.relative);
        let dst = r.target_home.join(&e.relative);
        if fs::symlink_metadata(&dst).is_ok() {
            ensure!(
                files::tree_digest(&dst)?.1 == e.digest,
                "TARGET_CONFLICT: {} already exists in the target home with different content",
                e.relative.display()
            );
            continue;
        }
        ensure!(
            files::tree_digest(&src)?.1 == e.digest,
            "HISTORY_CHANGED: {} changed after the process stopped",
            e.relative.display()
        );
        let stage = stage_root.join(i.to_string());
        if fs::symlink_metadata(&stage).is_ok() {
            files::remove_tree(&stage)?;
        }
        files::ensure_parent(&stage)?;
        files::copy_tree(&src, &stage)?;
        ensure!(
            files::tree_digest(&stage)?.1 == e.digest,
            "HISTORY_COPY_MISMATCH: {}",
            e.relative.display()
        );
        copy_times(&src, &stage);
        files::ensure_parent(&dst)?;
        fs::rename(&stage, &dst)?;
    }
    let _ = fs::remove_dir_all(&stage_root);
    let _ = fs::remove_dir(r.target_home.join(STAGING));
    phase(
        paths,
        r,
        "copied",
        "All files are present and verified in the target account home.",
    )
}

/// Pre-switch failure: remove only what this operation placed (digest-matched).
fn rollback(r: &Record) {
    if let Some(entries) = &r.entries {
        for e in entries {
            let dst = r.target_home.join(&e.relative);
            if files::tree_digest(&dst).is_ok_and(|(_, d)| d == e.digest) {
                let _ = files::remove_tree(&dst);
            }
        }
    }
    let _ = fs::remove_dir_all(r.target_home.join(STAGING).join(&r.id));
    let _ = fs::remove_dir(r.target_home.join(STAGING));
}

// ---------------------------------------------------------------------- switch

fn switch(paths: &Paths, r: &mut Record) -> Result<()> {
    phase(
        paths,
        r,
        "switching",
        "Pointing the session at the target account.",
    )?;
    let s = catalog::switch_account(
        paths,
        &r.session_id,
        &r.source_account,
        &r.target_account,
        &r.conversation_id,
    )?;
    r.generation_after = Some(s.generation());
    phase(
        paths,
        r,
        "switched",
        "The catalog now names the target account; removing the source copies.",
    )
}

fn remove(paths: &Paths, r: &mut Record) -> Result<()> {
    if r.shared {
        phase(
            paths,
            r,
            "removing",
            "Shared Claude state: nothing is deleted from the shared store; only dead registry entries of the source account are cleared.",
        )?;
        let ids: std::collections::BTreeSet<String> = r.conversation_ids.iter().cloned().collect();
        for stale in claude::dead_registry_entries(&r.source_home, &ids)? {
            let _ = fs::remove_file(stale);
        }
        return phase(
            paths,
            r,
            "removed",
            "Nothing was removed: the conversation stays in the shared store.",
        );
    }
    phase(
        paths,
        r,
        "removing",
        "Removing the verified source copies from the source account home.",
    )?;
    for e in entries(r)? {
        let src = r.source_home.join(&e.relative);
        if fs::symlink_metadata(&src).is_err() {
            continue;
        }
        ensure!(
            files::tree_digest(&src)?.1 == e.digest,
            "HISTORY_CHANGED: source {} changed before removal; it was kept",
            e.relative.display()
        );
        files::remove_tree(&src)?;
    }
    let ids: std::collections::BTreeSet<String> = r.conversation_ids.iter().cloned().collect();
    for stale in claude::dead_registry_entries(&r.source_home, &ids)? {
        let _ = fs::remove_file(stale);
    }
    phase(
        paths,
        r,
        "removed",
        "The source copies were removed; account memory and settings stay with the source account.",
    )
}

// ---------------------------------------------------------------------- resume

fn confirmed(paths: &Paths, r: &Record) -> bool {
    let Ok(s) = catalog::find(paths, &r.session_id) else {
        return false;
    };
    let Some(a) = &s.attachment else {
        return false;
    };
    s.account == r.target_account
        && Some(&a.target) == r.pane.as_ref()
        && claude::launch_observed(&s)
        && claude::live_sessions(&r.target_home).is_ok_and(|live| {
            live.iter()
                .any(|l| l.pid == a.pid && r.conversation_ids.contains(&l.session_id))
        })
}

async fn settle(paths: &Paths, r: &mut Record, wait: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    loop {
        if confirmed(paths, r) {
            r.resume = "confirmed".into();
            return phase(
                paths,
                r,
                "complete",
                "Conversation moved and resumed in its pane on the target account.",
            );
        }
        if std::time::Instant::now() >= deadline {
            r.resume = "unconfirmed".into();
            r.uncertain_from = Some("resuming".into());
            return phase(
                paths,
                r,
                "uncertain",
                "The resume was dispatched but the new process was not confirmed. It is never replayed: inspect the pane, then api recover to re-check or api abandon-operation.",
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn pane_command(paths: &Paths, r: &Record, generation: &str) -> Result<String> {
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
    Ok(if pane.starts_with("herdr/") {
        let (session, _) = crate::terminal::target(pane)?;
        format!(
            "env -u TMUX -u TMUX_PANE SESSIONSATION_HERDR_SESSION={} {quoted}",
            shell_quote(session)
        )
    } else {
        quoted
    })
}

async fn resume(paths: &Paths, r: &mut Record) -> Result<()> {
    if r.phase == "resuming" {
        // Evidence only: a dispatched resume is never replayed.
        return settle(paths, r, 5).await;
    }
    let (Some(pane), Some(_)) = (r.pane.clone(), r.process.clone()) else {
        r.resume = "not_needed".into();
        return phase(
            paths,
            r,
            "complete",
            "Conversation moved to the target account. It was not running, so it was not resumed.",
        );
    };
    phase(
        paths,
        r,
        "resume_pending",
        "Waiting for the pane's shell before resuming on the target account.",
    )?;
    let socket = crate::external::hosting::socket_for(paths, &pane)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(SHELL_SECONDS);
    loop {
        if crate::terminal::state_at(&pane, socket.as_deref()).is_ok_and(|(idle, _)| idle) {
            break;
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "PANE_NOT_AVAILABLE: the pane did not return to an idle shell; the resume was not dispatched"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let current = catalog::find(paths, &r.session_id)?;
    let command = pane_command(paths, r, &current.generation())?;
    let cwd = r.directory.clone().context("SAVED_DIRECTORY_UNAVAILABLE")?;
    r.resume = "dispatching".into();
    phase(
        paths,
        r,
        "resuming",
        "Resume dispatched to the pane with the target account's config home. It is never replayed.",
    )?;
    crate::terminal::relaunch_at(&pane, &cwd, &command, socket.as_deref())?;
    settle(paths, r, CONFIRM_SECONDS).await
}

// -------------------------------------------------------------- recover/abandon

/// Continue from the journaled phase. Stop is pidfd-bound to the verified
/// process, copy/switch/remove are idempotent, and a dispatched resume is only
/// re-checked, never re-sent.
pub async fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let lock = lock_file(&worker_lock_path(paths, id)?)?;
    crate::paths::try_lock_brief(&lock)
        .context("SESSION_OPERATION_IN_PROGRESS: the operation's worker is still running")?;
    let mut r = read(paths, id)?;
    if is_terminal(&r) {
        return Ok(public(paths, &r));
    }
    if r.phase == "uncertain" {
        let from = r
            .uncertain_from
            .clone()
            .unwrap_or_else(|| "admitted".into());
        r.uncertain_from = None;
        r.phase = from;
        save(paths, &mut r)?;
    }
    run(paths, &mut r).await;
    drop(lock);
    Ok(public(paths, &r))
}

/// Operator abandon of an uncertain or stuck switch (terminal; moves nothing).
pub fn abandon(paths: &Paths, id: &str, confirm: &str) -> Result<Value> {
    ensure!(id == confirm, "CONFIRMATION_MISMATCH");
    let mut r = read(paths, id)?;
    ensure!(!is_terminal(&r), "OPERATION_ALREADY_TERMINAL");
    ensure!(
        r.phase == "uncertain" || stalled(paths, &r),
        "OPERATION_NOT_STUCK"
    );
    let from = r.phase.clone();
    phase(
        paths,
        &mut r,
        "abandoned",
        &format!(
            "Abandoned by the operator from '{from}'. No file was moved or removed by this step; check both account homes before another switch."
        ),
    )?;
    Ok(public(paths, &r))
}

#[cfg(test)]
mod tests;
