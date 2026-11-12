//! Claude side of `set-model` (contract 1.49.0): type `/model <id>` and `/effort <level>` into
//! the session's verified HERDR pane, answer the confirmation dialog, and confirm each value
//! only from the status-line hook record.
//!
//! Screens captured from Claude Code 2.1.314 (`tests/fixtures/claude-screens/model-*`,
//! `effort-*`): a fully typed `/model <id>` or `/effort <level>` shows no menu above the input
//! box; the command answers with `⎿ Set model to <name> and saved as your default for new
//! sessions` (or `Model '<id>' not found`) and an empty prompt; with a warm prompt cache a
//! `Switch model?` dialog asks first (`❯ 1. Yes, switch to <name>` / `2. No, go back`).
//!
//! Rules: the prompt must be empty (a user draft is never cleared for a model change; the change
//! waits with reason `draft_present`); a key is sent only on a screen that was identified;
//! never Esc, Ctrl+C, Ctrl+D or a digit key; an unidentified screen after Enter ends the
//! operation (`TERMINAL_OVERLAY_UNIDENTIFIED`) with the screen layout, instead of stalling.
use super::*;
use crate::messaging::claude_steer::terminal::{self as term, Cell, Host, View, classify, squash};
use crate::provider::claude;
use crate::session_model::HookRecord;
use std::path::Path;
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_millis(150);
/// How long a submitted command may take to show its result in the hook record.
const CONFIRM_WAIT: Duration = Duration::from_secs(20);
/// An empty prompt with no hook change this long after Enter: Claude refused or substituted.
const REFUSED_AFTER: Duration = Duration::from_secs(8);
const TYPED_WAIT: Duration = Duration::from_secs(3);

pub(crate) fn runtime_key(t: &term::Terminal) -> String {
    format!("{}:{}:{}", t.pid, t.process_start, t.target)
}

/// Admission checks for a Claude session: terminal input on, a verified HERDR attachment, and a
/// hook record written by this session's own process (so the model can be verified at all).
pub(crate) fn admit(
    paths: &Paths,
    session: &crate::catalog::Session,
) -> anyhow::Result<(term::Terminal, HookRecord)> {
    ensure!(term::enabled(paths), "{}", term::DISABLED);
    let account = claude::account(paths, &session.account)?;
    let o = claude::observe(
        &account.home,
        &session.conversation_id,
        session.attachment.as_ref(),
    );
    ensure!(o.availability == "running", "SESSION_NOT_RUNNING");
    let t = term::eligible(&o).map_err(|code| anyhow::anyhow!("{code}"))?;
    let hook = current_hook(&account.home, &session.conversation_id, &o).context(
        "SESSION_STATE_UNVERIFIED: the session reports no model (started before contract 1.49, status line disabled, or folder trust not accepted); stop and resume it to enable set-model",
    )?;
    Ok((t, hook))
}

/// The hook record of this running process: written at or after its start.
fn current_hook(home: &Path, conversation: &str, o: &claude::Observation) -> Option<HookRecord> {
    let started = o
        .process_start
        .and_then(claude::process_started_at)
        .or(o.active_since)?;
    crate::session_model::hook_record(home, conversation)
        .filter(|h| h.model_id.is_some() && h.observed_at_ms / 1000 >= started - 2)
}

/// Re-observe the target: same process and pane as admitted.
fn target(paths: &Paths, r: &Record) -> std::result::Result<(term::Terminal, PathBuf), String> {
    if !term::enabled(paths) {
        return Err("terminal_input_disabled".into());
    }
    let account = claude::account(paths, &r.account).map_err(|_| "session_not_running")?;
    let session = crate::catalog::find(paths, &r.session_id).map_err(|_| "generation_changed")?;
    if session.conversation_id != r.conversation_id {
        return Err("generation_changed".into());
    }
    if crate::provider::claude::hosted::ensure_not_hosted(paths, &r.conversation_id).is_err() {
        return Err("generation_changed".into());
    }
    let o = claude::observe(
        &account.home,
        &r.conversation_id,
        session.attachment.as_ref(),
    );
    if o.availability != "running" {
        return Err("session_not_running".into());
    }
    let t = term::eligible(&o).map_err(str::to_owned)?;
    if runtime_key(&t) != r.runtime {
        return Err("generation_changed".into());
    }
    Ok((t, account.home))
}

pub async fn worker(paths: &Paths, id: &str) -> anyhow::Result<()> {
    let lock = lock_file(&worker_lock(paths, id)?)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let r = read(paths, id)?;
    if r.terminal() || r.provider != "claude" {
        return Ok(());
    }
    if r.phase == "waiting" && crate::daemon::now() >= r.expires_at {
        finish(paths, id, &["waiting"], "expired", "expired");
        return Ok(());
    }
    let (t, home) = match target(paths, &r) {
        Ok(x) => x,
        Err(code) if code == "generation_changed" => {
            finish(
                paths,
                id,
                &["waiting", "applying"],
                "failed",
                "generation_changed",
            );
            return Ok(());
        }
        Err(code) if r.phase == "waiting" => {
            waiting(paths, id, &code);
            return Ok(());
        }
        Err(_) => {
            fail(paths, id, "worker_interrupted", None);
            return Ok(());
        }
    };
    // One terminal procedure per pane at a time (shared with message deliveries).
    let steer = paths.root.join(crate::layout::CLAUDE_STEER);
    private_dir(&steer)?;
    let key: String = t
        .target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let pane_lock = lock_file(&steer.join(format!("terminal-{key}.lock")))?;
    if pane_lock.try_lock_exclusive().is_err() {
        if r.phase == "waiting" {
            waiting(paths, id, "session_busy");
        }
        return Ok(());
    }
    let host = Host::for_agent(&t, "claude")?;
    if r.phase == "waiting" {
        if let Err(why) = gates(&host) {
            waiting(paths, id, why);
            return Ok(());
        }
        let started = update(paths, id, |x| {
            if x.phase != "waiting" || crate::daemon::now() >= x.expires_at {
                return false;
            }
            x.phase = "applying".into();
            x.reason = None;
            x.attempts += 1;
            x.started_at = Some(crate::daemon::now());
            true
        })?;
        if started.is_none() {
            return Ok(());
        }
    } else {
        // A worker stopped mid-procedure: continue only from a screen that is understood.
        let rows = host.screen()?;
        let clean = matches!(classify(&rows, true), Ok(View::Main(ref d)) if d.is_empty());
        if !clean && dialog(&rows).is_none() {
            refuse(paths, id, "worker_interrupted", &rows);
            return Ok(());
        }
    }
    let r = read(paths, id)?;
    match apply(paths, &r, &host, &home).await {
        Ok(done) => complete(paths, id, "completed", done),
        Err((reason, rows)) => {
            let observed = crate::session_model::hook_record(&home, &r.conversation_id)
                .as_ref()
                .and_then(Observed::from_hook);
            if let Some(rows) = rows {
                let layout = term::layout(&rows);
                let _ = update(paths, id, |x| {
                    x.refusal_layout = Some(layout);
                    true
                });
            }
            let retry = matches!(reason, "TERMINAL_TYPED_TEXT_MISMATCH" | "session_busy")
                && r.attempts < MAX_ATTEMPTS;
            if retry {
                let _ = update(paths, id, |x| {
                    if x.phase != "applying" {
                        return false;
                    }
                    x.phase = "waiting".into();
                    x.reason = Some(reason.into());
                    true
                });
            } else {
                fail(paths, id, reason, observed);
            }
        }
    }
    drop(pane_lock);
    Ok(())
}

fn refuse(paths: &Paths, id: &str, reason: &str, rows: &[Vec<Cell>]) {
    let layout = term::layout(rows);
    let _ = update(paths, id, |x| {
        x.refusal_layout = Some(layout);
        true
    });
    fail(paths, id, reason, None);
}

/// Idle with an empty main prompt.
fn gates(host: &Host) -> std::result::Result<(), &'static str> {
    match host.agent_status() {
        Ok(Some(s)) if s == "idle" || s == "done" => {}
        Ok(Some(s)) if s == "working" => return Err("turn_active"),
        Ok(Some(s)) if s == "blocked" => return Err("approval_or_user_input_required"),
        Ok(_) => return Err("state_unverified"),
        Err(_) => return Err("TERMINAL_HOST_UNAVAILABLE"),
    }
    let rows = host.screen().map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
    match classify(&rows, true) {
        Ok(View::Main(d)) if d.is_empty() => Ok(()),
        Ok(View::Main(_)) => Err("draft_present"),
        Ok(_) => Err("TERMINAL_VIEW_NOT_MAIN"),
        Err(code) => Err(code),
    }
}

type Failure = (&'static str, Option<Vec<Vec<Cell>>>);

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Kind {
    Model,
    Effort,
}

async fn apply(
    paths: &Paths,
    r: &Record,
    host: &Host,
    home: &Path,
) -> std::result::Result<Observed, Failure> {
    let hook = || crate::session_model::hook_record(home, &r.conversation_id);
    let observed = |h: &HookRecord| Observed::from_hook(h);
    let model_ok = |h: &HookRecord| {
        h.model_id
            .as_deref()
            .is_some_and(|m| crate::session_model::same(m, &r.requested_model))
    };
    // 1. Model (skipped when the session already reports it).
    if !hook().is_some_and(|h| model_ok(&h)) {
        step(paths, &r.id, "model_typed");
        run_command(
            paths,
            r,
            host,
            &format!("/model {}", r.requested_model),
            Kind::Model,
            &|h| model_ok(h),
        )
        .await?;
    }
    // 2. Effort, always after the model.
    let target = target_effort(r);
    let Some(effort) = target else {
        let _ = update(paths, &r.id, |x| {
            x.effort_source = "model_default".into();
            true
        });
        return hook()
            .as_ref()
            .and_then(observed)
            .ok_or(("model_not_confirmed", None));
    };
    let effort_ok = |h: &HookRecord| {
        model_ok(h)
            && h.effort_level
                .as_deref()
                .is_some_and(|e| crate::session_model::same(e, &effort))
    };
    match hook() {
        Some(h) if effort_ok(&h) => {}
        Some(h) if h.effort_level.is_none() => return Err(("effort_unsupported", None)),
        _ => {
            step(paths, &r.id, "effort_typed");
            run_command(
                paths,
                r,
                host,
                &format!("/effort {effort}"),
                Kind::Effort,
                &|h| effort_ok(h),
            )
            .await?;
        }
    }
    hook()
        .filter(effort_ok)
        .as_ref()
        .and_then(observed)
        .ok_or(("effort_not_confirmed", None))
}

/// Type one command into the empty prompt, verify it on screen, submit it, answer its
/// confirmation dialog, and wait for the hook record to show `done`.
async fn run_command(
    paths: &Paths,
    r: &Record,
    host: &Host,
    command: &str,
    kind: Kind,
    done: &dyn Fn(&HookRecord) -> bool,
) -> std::result::Result<(), Failure> {
    let home_hook = || {
        claude::account(paths, &r.account)
            .ok()
            .and_then(|a| crate::session_model::hook_record(&a.home, &r.conversation_id))
    };
    let screen = |host: &Host| {
        host.screen()
            .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", None))
    };
    let rows = screen(host)?;
    if let Some(d) = dialog(&rows) {
        // A dialog left open by a stopped worker: answer it and go on confirming.
        answer(host, d, kind).await?;
    } else {
        match classify(&rows, true) {
            Ok(View::Main(d)) if d.is_empty() => {}
            Ok(View::Main(_)) => return Err(("TERMINAL_DRAFT_CHANGED", Some(rows))),
            Ok(_) => return Err(("TERMINAL_VIEW_NOT_MAIN", Some(rows))),
            Err(code) => return Err((code, Some(rows))),
        }
        host.text(command)
            .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", None))?;
        let deadline = Instant::now() + crate::timing::scaled(TYPED_WAIT);
        let mut last;
        loop {
            tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
            last = screen(host)?;
            if command_view(&last, command) {
                break;
            }
            if Instant::now() >= deadline {
                withdraw(host).await;
                return Err(("TERMINAL_TYPED_TEXT_MISMATCH", Some(last)));
            }
        }
        step(
            paths,
            &r.id,
            if kind == Kind::Model {
                "model_submitted"
            } else {
                "effort_submitted"
            },
        );
        host.keys(&["enter"])
            .map_err(|_| ("TERMINAL_SUBMIT_UNCERTAIN", None))?;
    }
    let submitted = Instant::now();
    let deadline = submitted + crate::timing::scaled(CONFIRM_WAIT);
    let mut answered = 0;
    let mut last = vec![];
    loop {
        tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
        if home_hook().is_some_and(|h| done(&h)) {
            step(
                paths,
                &r.id,
                if kind == Kind::Model {
                    "model_verified"
                } else {
                    "effort_verified"
                },
            );
            return Ok(());
        }
        last = screen(host)?;
        if let Some(d) = dialog(&last) {
            if answered >= 2 {
                return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", Some(last)));
            }
            answered += 1;
            step(
                paths,
                &r.id,
                if kind == Kind::Model {
                    "model_confirm"
                } else {
                    "effort_confirm"
                },
            );
            answer(host, d, kind).await?;
            continue;
        }
        let clean = matches!(classify(&last, true), Ok(View::Main(ref d)) if d.is_empty());
        if clean && submitted.elapsed() >= crate::timing::scaled(REFUSED_AFTER) {
            break;
        }
        if Instant::now() >= deadline {
            if !clean {
                return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", Some(last)));
            }
            break;
        }
    }
    Err((
        if kind == Kind::Model {
            "model_not_confirmed"
        } else {
            "effort_not_confirmed"
        },
        Some(last),
    ))
}

/// Remove the text this worker typed (it is the only content of the prompt).
async fn withdraw(host: &Host) {
    for _ in 0..4 {
        let _ = host.keys(&["ctrl+k", "ctrl+u"]);
    }
    tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
}

/// The prompt holds exactly `command`, with no menu or overlay drawn (the screen classifier
/// refuses a menu row above the box).
pub(crate) fn command_view(rows: &[Vec<Cell>], command: &str) -> bool {
    matches!(classify(rows, false), Ok(View::Main(d)) if squash(&d) == squash(command))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Pointer {
    Yes,
    No,
    None,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Dialog {
    pub kind: Kind,
    pub pointer: Pointer,
    pub hook_asked: bool,
}

/// A `Switch model?` / `Change effort level?` confirmation: exactly one title row, one `Yes…`
/// option and one `No…` option below it (an optional `❯` pointer and `N.` index before each).
pub(crate) fn dialog(rows: &[Vec<Cell>]) -> Option<Dialog> {
    let lines: Vec<String> = rows.iter().map(|r| term::row_text(r)).collect();
    let titles: Vec<(usize, Kind)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| match l.trim() {
            "Switch model?" => Some((i, Kind::Model)),
            "Change effort level?" => Some((i, Kind::Effort)),
            _ => None,
        })
        .collect();
    let [(title, kind)] = titles.as_slice() else {
        return None;
    };
    let option = |l: &str| -> Option<(bool, bool)> {
        let t = l.trim_start();
        let pointed = t.starts_with('❯');
        let rest = t.trim_start_matches('❯').trim_start();
        let rest = match rest.split_once(". ") {
            Some((n, r)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => r,
            _ => rest,
        };
        if rest.starts_with("Yes") {
            Some((true, pointed))
        } else if rest.starts_with("No") {
            Some((false, pointed))
        } else {
            None
        }
    };
    let below: Vec<(bool, bool)> = lines[title + 1..]
        .iter()
        .take(16)
        .filter_map(|l| option(l))
        .collect();
    let yes: Vec<&(bool, bool)> = below.iter().filter(|o| o.0).collect();
    let no: Vec<&(bool, bool)> = below.iter().filter(|o| !o.0).collect();
    if yes.len() != 1 || no.len() != 1 {
        return None;
    }
    let pointer = match (yes[0].1, no[0].1) {
        (true, false) => Pointer::Yes,
        (false, true) => Pointer::No,
        _ => Pointer::None,
    };
    let hook_asked = lines[title + 1..]
        .iter()
        .take(16)
        .any(|l| l.contains("PreModelSwitch hook"));
    Some(Dialog {
        kind: *kind,
        pointer,
        hook_asked,
    })
}

/// Answer a dialog: Yes (moving the pointer with up/down only), or No when a hook asked.
async fn answer(host: &Host, d: Dialog, kind: Kind) -> std::result::Result<(), Failure> {
    let reread = || async {
        tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
        host.screen().ok().and_then(|rows| dialog(&rows))
    };
    if d.kind != kind {
        return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", host.screen().ok()));
    }
    let key = |k: &str| {
        host.keys(&[k])
            .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", None::<Vec<Vec<Cell>>>))
    };
    if d.hook_asked {
        if d.pointer == Pointer::Yes {
            key("down")?;
            if reread().await.map(|d| d.pointer) != Some(Pointer::No) {
                return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", host.screen().ok()));
            }
        } else if d.pointer != Pointer::No {
            return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", host.screen().ok()));
        }
        key("enter")?;
        return Err(("blocked_by_hook", None));
    }
    match d.pointer {
        Pointer::Yes => {}
        Pointer::No => {
            key("up")?;
            if reread().await.map(|d| d.pointer) != Some(Pointer::Yes) {
                return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", host.screen().ok()));
            }
        }
        Pointer::None => return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", host.screen().ok())),
    }
    key("enter")?;
    Ok(())
}
