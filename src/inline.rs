//! Contract 1.32: journaled `sessionsation run`/`resume` typed into an existing
//! terminal. They write the same launch/resumption receipts as `api launch`/
//! `api resume-launch` (marked `inline`), appear in `api operations`, take the
//! same admission locks, and treat an unclear outcome as `uncertain` (never replayed).
use crate::{catalog, daemon, launch, lifecycle, paths::Paths, provider::ProviderId, resume};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The terminal process that runs the provider (it execs into it, keeping pid and start time).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub pid: u32,
    pub process_start: u64,
    /// HERDR pane the command was typed into, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

fn info() -> Result<Info> {
    let pid = std::process::id();
    Ok(Info {
        pid,
        process_start: crate::external::process_start(pid)?,
        pane_id: std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|p| !p.is_empty()),
    })
}
fn alive(info: &Info) -> bool {
    crate::external::process_start(info.pid).is_ok_and(|start| start == info.process_start)
}
fn host() -> launch::Host {
    launch::Host {
        session: "inline".into(),
        socket: "/".into(),
    }
}
fn terminal(phase: &str) -> bool {
    matches!(
        phase,
        "complete" | "running" | "failed" | "recovered" | "abandoned"
    )
}

/// Rename the HERDR tab of the invoking pane, as the hosted launch paths label tabs.
/// Best effort: the catalog name stays authoritative.
pub fn label_tab(name: &str) {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        return;
    }
    let (Some(session), Ok(pane)) = (
        crate::terminal::herdr_session_from(|k| std::env::var(k).ok()),
        std::env::var("HERDR_PANE_ID"),
    ) else {
        return;
    };
    let Ok(found) = crate::terminal::call(&session, &["pane", "get", &pane]) else {
        return;
    };
    let tab = found
        .get("pane")
        .unwrap_or(&found)
        .get("tab_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(tab) = tab {
        let _ = crate::terminal::call(&session, &["tab", "rename", &tab, name]);
    }
}

/// Journal an inline new session. Returns the request (= future catalog session) id.
pub fn begin_new(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
    name: &str,
    cwd: &Path,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let info = info()?;
    let _lock = launch::lock(paths)?;
    catalog::reserve_name(paths, name, &id)?;
    let now = daemon::now();
    let mut receipt = launch::Receipt {
        version: 1,
        request: launch::Request {
            version: 1,
            request_id: id.clone(),
            account_id: format!("{}:{account}", provider.as_str()),
            name: name.into(),
            host: host(),
            space: launch::Space::Existing {
                id: "inline".into(),
            },
            directory: launch::Directory {
                path: cwd.into(),
                create: vec![],
                confirmed_existing: true,
            },
            hosted: None,
            permissions: None,
            model: None,
            reasoning_effort: None,
        },
        phase: "starting_provider".into(),
        detail: None,
        workspace_id: None,
        tab_id: None,
        pane_id: info.pane_id.clone(),
        conversation_id: None,
        session_id: None,
        generation: None,
        created_directories: vec![],
        created_at: Some(now),
        updated_at: now,
        inline: Some(info),
    };
    if let Err(error) = launch::save(paths, &mut receipt) {
        let _ = catalog::release_reservation(paths, &id, name);
        return Err(error);
    }
    Ok(id)
}

/// Journal an inline resume under the same locks `api resume-launch` takes.
pub fn begin_resume(
    paths: &Paths,
    s: &catalog::Session,
    name: &str,
    confirm_unfinished: bool,
    cwd: &Path,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let info = info()?;
    settle_resumptions(paths);
    let _lock = resume::lock(paths)?;
    ensure!(
        !resume::records(paths)?
            .iter()
            .any(|prior| prior.request.session_id == s.id && !terminal(&prior.phase)),
        "RESUME_IN_PROGRESS"
    );
    {
        // Held only for admission, like `api resume-launch`; the provider path
        // admits again under this request id.
        let _admission = lifecycle::admit(paths, &s.conversation_id, Some(&id))?;
        lifecycle::ensure_no_handoff(paths, &s.conversation_id, Some(&id))?;
        let now = daemon::now();
        let mut receipt = resume::Receipt {
            version: 1,
            request: resume::Request {
                version: 1,
                request_id: id.clone(),
                session_id: s.id.clone(),
                expected_generation: s.generation(),
                account_id: format!("{}:{}", s.provider, s.account),
                name: name.into(),
                host: host(),
                space: launch::Space::Existing {
                    id: "inline".into(),
                },
                directory: None,
                confirm_unfinished,
                permissions: None,
            },
            directory: s
                .directory
                .clone()
                .map_or_else(|| cwd.to_path_buf(), PathBuf::from),
            saved_directory: None,
            created_directories: vec![],
            previous_name: s.name.clone(),
            conversation_id: s.conversation_id.clone(),
            generation: s.generation(),
            phase: "starting_provider".into(),
            detail: None,
            workspace_id: None,
            tab_id: None,
            pane_id: info.pane_id.clone(),
            operation_id: None,
            abandoned_from: None,
            abandoned_at: None,
            created_at: Some(now),
            updated_at: now,
            inline: Some(info),
        };
        resume::save(paths, &mut receipt)?;
    }
    lifecycle::set_inline_owner(&id);
    Ok(id)
}

/// The resumed session was renamed; keep the journal's view of it current.
pub fn renamed(paths: &Paths, id: &str, s: &catalog::Session) {
    let _ = resume::change(paths, id, |r| {
        r.generation = s.generation();
        r.request.expected_generation = s.generation();
        r.request.name = s.name.clone();
        Ok(())
    });
}

/// Called by a native provider once the session is bound to this terminal.
pub(crate) fn mark_attached(paths: &Paths) {
    if let Some(id) = lifecycle::inline_owner() {
        let _ = resume::change(paths, id, |r| {
            if r.inline.is_some() && r.phase == "starting_provider" {
                r.phase = "attached".into();
            }
            Ok(())
        });
    }
}

/// The provider did not take over the terminal. Before the session was bound
/// nothing started (`failed`, name released); after that the outcome is unclear
/// (`uncertain`, kept, never replayed).
pub fn fail_new(paths: &Paths, id: &str, error: &anyhow::Error) {
    let _ = launch::change(paths, id, |r| {
        if !terminal(&r.phase) && r.phase != "uncertain" {
            if r.phase == "starting_provider" {
                catalog::release_reservation(paths, id, &r.request.name)?;
                r.phase = "failed".into();
            } else {
                r.phase = "uncertain".into();
            }
            r.detail = Some(format!("{error:#}"));
        }
        Ok(())
    });
}
pub fn fail_resume(paths: &Paths, id: &str, error: &anyhow::Error) {
    let _ = resume::change(paths, id, |r| {
        if !terminal(&r.phase) && r.phase != "uncertain" {
            r.phase = if r.phase == "starting_provider" {
                "failed"
            } else {
                "uncertain"
            }
            .into();
            r.detail = Some(format!("{error:#}"));
        }
        Ok(())
    });
}

const NEVER_ATTACHED: &str = "The terminal process ended before the provider was attached. Nothing was resumed and no input was sent.";
const NEVER_ATTACHED_LEGACY: &str = "The terminal process ended before the provider was attached. Inspect the session; it is not replayed.";
const OBSERVED: &str = "Exact provider process observed; no launch replay";
const ENDED: &str = "The inline provider process has ended.";
/// Ended inline entries leave `api operations` after this many seconds.
const AGE_OUT: i64 = 24 * 3600;

/// Unsettled entries always show; `running` only while its session exists and the
/// process is alive or ended recently; other finished entries only recently.
fn visible(phase: &str, session_exists: bool, updated_at: i64) -> bool {
    if !terminal(phase) {
        return true;
    }
    let recent = daemon::now() - updated_at <= AGE_OUT;
    if phase == "complete" || phase == "running" {
        session_exists && recent
    } else {
        recent
    }
}

/// Next phase of an inline receipt from what is observable now.
fn next(
    phase: &str,
    detail: Option<&str>,
    updated_at: i64,
    info: &Info,
    observed: bool,
    resume: bool,
) -> Option<(&'static str, &'static str)> {
    // `complete` is the terminal success phase: the exact provider process was observed (or
    // has since ended). `running` is only read from receipts written by 1.33 - 1.35.0.
    match (phase, alive(info)) {
        ("running", true) => Some(("complete", OBSERVED)),
        ("running", false) if daemon::now() - updated_at <= AGE_OUT => Some(("complete", ENDED)),
        ("complete", false) if detail != Some(ENDED) && daemon::now() - updated_at <= AGE_OUT => {
            Some(("complete", ENDED))
        }
        // A resume whose terminal process ended before any provider attached started
        // nothing and sent no input: it failed, so the session is not left blocked.
        // (Receipts an earlier release settled as uncertain for this reason follow.)
        ("starting_provider", false) if resume => Some(("failed", NEVER_ATTACHED)),
        ("uncertain", false) if resume && detail == Some(NEVER_ATTACHED_LEGACY) => {
            Some(("failed", NEVER_ATTACHED))
        }
        ("starting_provider", false) => Some(("uncertain", NEVER_ATTACHED_LEGACY)),
        ("attached", false) => Some(("complete", ENDED)),
        ("attached", true) if observed => Some(("complete", OBSERVED)),
        _ => None,
    }
}

/// Settle one inline resume receipt. No journal lock is taken (callers may hold it).
pub(crate) fn settle_resume(paths: &Paths, id: &str) -> Result<()> {
    let mut r = resume::read(paths, id)?;
    let Some(info) = r.inline.clone() else {
        return Ok(());
    };
    let observed = r.phase == "attached"
        && catalog::list(paths)?
            .iter()
            .any(|s| s.id == r.request.session_id && launch::launch_observed(paths, s));
    if let Some((phase, detail)) = next(
        &r.phase,
        r.detail.as_deref(),
        r.updated_at,
        &info,
        observed,
        true,
    ) {
        r.phase = phase.into();
        r.detail = Some(detail.into());
        resume::save(paths, &mut r)?;
    }
    Ok(())
}
pub(crate) fn settle_launch(paths: &Paths, id: &str) -> Result<()> {
    let mut r = launch::status(paths, id)?;
    let Some(info) = r.inline.clone() else {
        return Ok(());
    };
    let observed = r.phase == "attached"
        && catalog::list(paths)?
            .iter()
            .any(|s| s.id == id && launch::launch_observed(paths, s));
    if let Some((phase, detail)) = next(
        &r.phase,
        r.detail.as_deref(),
        r.updated_at,
        &info,
        observed,
        false,
    ) {
        if phase == "uncertain" {
            // Never attached: the name stays reserved with the uncertain journal.
        }
        r.phase = phase.into();
        r.detail = Some(detail.into());
        launch::save(paths, &mut r)?;
    }
    Ok(())
}
fn journal_ids(paths: &Paths, directory: &str) -> Vec<String> {
    let dir = paths.root.join(directory);
    if !dir.is_dir() {
        return vec![];
    }
    crate::paths::journal_paths(&dir)
        .unwrap_or_default()
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
        .filter(|s| uuid::Uuid::parse_str(s).is_ok())
        .map(str::to_owned)
        .collect()
}
pub(crate) fn settle_resumptions(paths: &Paths) {
    for id in journal_ids(paths, crate::layout::RESUMPTIONS) {
        let _ = settle_resume(paths, &id);
    }
}

/// Inline launches and resumptions for `api operations`.
pub fn operations(paths: &Paths) -> Result<Vec<Value>> {
    let mut out = vec![];
    let sessions: Vec<String> = catalog::list(paths)?.into_iter().map(|s| s.id).collect();
    for id in journal_ids(paths, crate::layout::LAUNCHES) {
        if let Ok(r) = launch::status(paths, &id)
            && r.inline.is_some()
            && !visible(
                &r.phase,
                sessions.contains(r.session_id.as_ref().unwrap_or(&id)),
                r.updated_at,
            )
        {
            continue; // aged out or deleted: the receipt is left untouched
        }
        let _ = settle_launch(paths, &id);
        if let Ok(r) = launch::status(paths, &id)
            && r.inline.is_some()
            && visible(
                &r.phase,
                sessions.contains(r.session_id.as_ref().unwrap_or(&id)),
                r.updated_at,
            )
        {
            out.push(json!({"id":id,"kind":"launch","inline":true,"session_id":r.session_id.clone().or_else(||(r.phase!="failed").then(||id.clone())),
                "name":r.request.name,"account_id":r.request.account_id,"phase":r.phase,"detail":r.detail,
                "created_at":r.created_at,"updated_at":r.updated_at,"pane_id":r.pane_id,
                "can_cancel":false,"can_respond":false,"can_recover":false}));
        }
    }
    for id in journal_ids(paths, crate::layout::RESUMPTIONS) {
        let _ = settle_resume(paths, &id);
        if let Ok(r) = resume::read(paths, &id)
            && r.inline.is_some()
            && visible(
                &r.phase,
                sessions.contains(&r.request.session_id),
                r.updated_at,
            )
        {
            out.push(json!({"id":id,"kind":"resume","inline":true,"session_id":r.request.session_id,
                "name":r.request.name,"account_id":r.request.account_id,"phase":r.phase,"detail":r.detail,
                "created_at":r.created_at,"updated_at":r.updated_at,"pane_id":r.pane_id,
                "can_cancel":false,"can_respond":false,"can_recover":false}));
        }
    }
    Ok(out)
}

/// `api capabilities.inline_launch` (revision 32).
pub fn capability() -> Value {
    json!({"since_revision":32,"version":1,"commands":["run","resume"],"name":true,"name_option":"--name",
        "name_rules":"same as api launch / api resume --name","renames_herdr_tab":true,
        "journaled":true,"journal":["launches","resumptions"],"operations":true,"operation_kinds":["launch","resume"],
        "locks":["account_launch","conversation_admission","conversation"],"uncertain_never_replayed":true,
        "providers":["codex","claude","gemini","antigravity"],"uncovered":["run --thread","run --hosted","run --host herdr"]})
}

/// Default session name for an unnamed inline new session, unique in the catalog.
pub fn default_name(paths: &Paths, provider: ProviderId, cwd: &Path) -> Result<String> {
    let label = match provider {
        ProviderId::Codex => crate::external::default_new_name(cwd)?,
        other => cwd
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(match other {
                ProviderId::Claude => "claude",
                ProviderId::Gemini => "gemini",
                _ => "antigravity",
            })
            .to_owned(),
    };
    catalog::free_name(paths, &label)
}

/// Journaled inline new session: optional `--name`, tab label, then the provider
/// takes over this process. Returns only when the provider did not start.
pub async fn run_new(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
    name: Option<&str>,
    options: &[String],
) -> Result<()> {
    if let Some(name) = name {
        launch::validate_session_name(name)?;
    }
    let cwd = std::env::current_dir()?;
    ensure!(cwd.is_dir(), "working directory is not a directory");
    let _account = crate::accounts::launch_lock(paths, provider, account)?;
    let mut attempt = 0;
    let (id, name) = loop {
        let chosen = match name {
            Some(name) => name.to_owned(),
            None => default_name(paths, provider, &cwd)?,
        };
        match begin_new(paths, provider, account, &chosen, &cwd) {
            Ok(id) => break (id, chosen),
            // An unnamed launch races only other unnamed launches for the free name.
            Err(error)
                if name.is_none()
                    && attempt < 5
                    && format!("{error:#}").contains("SESSION_NAME_EXISTS") =>
            {
                attempt += 1
            }
            Err(error) => return Err(error),
        }
    };
    label_tab(&name);
    let result = match provider {
        ProviderId::Codex => {
            crate::external::run_account_new_named_options(paths, account, &id, &name, options)
                .await
        }
        ProviderId::Claude => {
            crate::provider::claude::launch::exec_new(paths, account, None, Some(&id)).await
        }
        ProviderId::Gemini => {
            crate::provider::gemini::launch::exec_new(paths, account, None, Some(&id)).await
        }
        ProviderId::Antigravity => {
            crate::provider::antigravity::launch::exec_new(paths, account, None, Some(&id)).await
        }
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            fail_new(paths, &id, &error);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead() -> Info {
        Info {
            pid: 999_999_999,
            process_start: 1,
            pane_id: None,
        }
    }
    fn receipt(conversation: &str, phase: &str, detail: Option<&str>) -> resume::Receipt {
        resume::Receipt {
            version: 1,
            request: resume::Request {
                version: 1,
                request_id: uuid::Uuid::new_v4().to_string(),
                session_id: uuid::Uuid::new_v4().to_string(),
                expected_generation: "g".into(),
                account_id: "codex:a".into(),
                name: "agent-1".into(),
                host: host(),
                space: launch::Space::Existing {
                    id: "inline".into(),
                },
                directory: None,
                confirm_unfinished: false,
                permissions: None,
            },
            directory: "/tmp".into(),
            saved_directory: None,
            created_directories: vec![],
            previous_name: "agent-1".into(),
            conversation_id: conversation.into(),
            generation: "g".into(),
            phase: phase.into(),
            detail: detail.map(str::to_owned),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            operation_id: None,
            abandoned_from: None,
            abandoned_at: None,
            created_at: Some(daemon::now()),
            updated_at: daemon::now(),
            inline: Some(dead()),
        }
    }

    #[test]
    fn a_resume_that_never_attached_settles_failed_and_unblocks_the_session() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("state"))).unwrap();
        let conversation = uuid::Uuid::new_v4().to_string();
        let mut r = receipt(&conversation, "starting_provider", None);
        let id = r.request.request_id.clone();
        resume::save(&paths, &mut r).unwrap();
        // Reading the session's operations settles the dead inline resume.
        assert_eq!(
            lifecycle::pending(&paths, &conversation, None).unwrap(),
            None
        );
        let settled = resume::read(&paths, &id).unwrap();
        assert_eq!(settled.phase, "failed");
        assert_eq!(settled.detail.as_deref(), Some(NEVER_ATTACHED));
        lifecycle::ensure_available(&paths, &conversation, None).unwrap();
    }

    #[test]
    fn a_legacy_uncertain_never_attached_resume_settles_failed_but_other_uncertain_stays() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("state"))).unwrap();
        let conversation = uuid::Uuid::new_v4().to_string();
        let mut legacy = receipt(&conversation, "uncertain", Some(NEVER_ATTACHED_LEGACY));
        let mut other = receipt(
            &conversation,
            "uncertain",
            Some("The resume was dispatched"),
        );
        let (legacy_id, other_id) = (
            legacy.request.request_id.clone(),
            other.request.request_id.clone(),
        );
        resume::save(&paths, &mut legacy).unwrap();
        resume::save(&paths, &mut other).unwrap();
        // The unrelated uncertain resume still blocks, and is named with its id.
        let blocked = lifecycle::pending_detail(&paths, &conversation, None)
            .unwrap()
            .unwrap();
        assert_eq!(blocked.id, other_id);
        assert_eq!(
            (blocked.kind, blocked.phase.as_str()),
            ("resume", "uncertain")
        );
        let error = lifecycle::ensure_available(&paths, &conversation, None)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("RECOVERY_UNRESOLVED: operation "),
            "{error}"
        );
        assert!(error.contains(&other_id), "{error}");
        assert_eq!(resume::read(&paths, &legacy_id).unwrap().phase, "failed");
        // An in-progress phase keeps the retryable code, still with the id.
        let mut running = receipt(&uuid::Uuid::new_v4().to_string(), "dispatching", None);
        let (running_id, running_conversation) = (
            running.request.request_id.clone(),
            running.conversation_id.clone(),
        );
        running.inline = None;
        resume::save(&paths, &mut running).unwrap();
        let error = lifecycle::ensure_available(&paths, &running_conversation, None)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("SESSION_OPERATION_IN_PROGRESS: operation "),
            "{error}"
        );
        assert!(error.contains(&running_id), "{error}");
    }
}
