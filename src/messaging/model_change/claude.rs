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
//! never Ctrl+C, Ctrl+D or a digit key (Esc only to close its own confirmation dialog when its
//! `No` option cannot be chosen); an unidentified screen after Enter ends the operation
//! (`TERMINAL_OVERLAY_UNIDENTIFIED`) with the screen layout, instead of stalling, and a dialog is
//! never left open (`pane_left_in_dialog` names the pane when closing it cannot be verified).
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

/// The pane operations the procedure needs; HERDR in production, a scripted fake in tests.
pub(crate) trait Pane {
    fn agent_status(&self) -> anyhow::Result<Option<String>>;
    fn screen(&self) -> anyhow::Result<Vec<Vec<Cell>>>;
    fn keys(&self, keys: &[&str]) -> anyhow::Result<()>;
    fn text(&self, s: &str) -> anyhow::Result<()>;
}
struct Herdr(Host);
impl Pane for Herdr {
    fn agent_status(&self) -> anyhow::Result<Option<String>> {
        self.0.agent_status()
    }
    fn screen(&self) -> anyhow::Result<Vec<Vec<Cell>>> {
        self.0.screen()
    }
    fn keys(&self, keys: &[&str]) -> anyhow::Result<()> {
        self.0.keys(keys)
    }
    fn text(&self, s: &str) -> anyhow::Result<()> {
        self.0.text(s)
    }
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
    let pane = Herdr(Host::for_agent(&t, "claude")?);
    procedure(paths, id, &pane, &home, &t.target).await?;
    drop(pane_lock);
    Ok(())
}

/// Everything after the target and pane lock are verified (testable with a fake pane).
pub(crate) async fn procedure(
    paths: &Paths,
    id: &str,
    pane: &dyn Pane,
    home: &Path,
    target: &str,
) -> anyhow::Result<()> {
    let r = read(paths, id)?;
    if r.phase == "waiting" {
        if let Err(why) = ready_twice(paths, &r, pane).await {
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
        // A worker stopped mid-procedure: continue only from a screen that is understood (an
        // empty prompt, its own confirmation dialog, or its own command left typed).
        let rows = pane.screen()?;
        let clean = matches!(classify(&rows, true), Ok(View::Main(ref d)) if d.is_empty());
        let own = own_command(&rows, &r).is_some();
        if !clean && !own && dialog_title(&rows).is_none() {
            refuse(paths, id, "worker_interrupted", &rows);
            return Ok(());
        }
    }
    let r = read(paths, id)?;
    let outcome = apply(paths, &r, pane, home).await;
    let Err((reason, rows)) = outcome else {
        let Ok(done) = outcome else { unreachable!() };
        complete(paths, id, "completed", done);
        return Ok(());
    };
    // Never leave a confirmation dialog open on a failure.
    let (reason, detail) = match close_dialog(pane).await {
        Ok(()) => (reason, None),
        Err(()) => (
            "pane_left_in_dialog",
            Some(format!(
                "pane {target} may still show a confirmation dialog after {reason}; answer it there (No) before typing"
            )),
        ),
    };
    let observed = crate::session_model::hook_record(home, &r.conversation_id)
        .as_ref()
        .and_then(Observed::from_hook);
    let layout = rows.map(|rows| term::layout(&rows));
    let _ = update(paths, id, |x| {
        if layout.is_some() {
            x.refusal_layout = layout;
        }
        if detail.is_some() {
            x.detail = detail;
        }
        true
    });
    let retry = matches!(
        reason,
        "TERMINAL_TYPED_TEXT_MISMATCH" | "session_busy" | "terminal_delivery_active"
    ) && r.attempts < MAX_ATTEMPTS;
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

/// Seconds after a terminal delivery touched the pane before a model change may type.
const DELIVERY_QUIET_SECONDS: i64 = 5;
/// Two idle observations at least this far apart.
const IDLE_GAP: Duration = Duration::from_millis(1500);

/// Idle twice, at least `IDLE_GAP` apart, with no terminal delivery in flight or just done:
/// closes the race with a delivery that has just pressed Enter.
async fn ready_twice(
    paths: &Paths,
    r: &Record,
    pane: &dyn Pane,
) -> std::result::Result<(), &'static str> {
    let quiet = || {
        if crate::messaging::claude_steer::terminal_activity(
            paths,
            &r.conversation_id,
            DELIVERY_QUIET_SECONDS,
        ) {
            Err("terminal_delivery_active")
        } else {
            Ok(())
        }
    };
    quiet()?;
    gates(pane)?;
    tokio::time::sleep(crate::timing::scaled(IDLE_GAP)).await;
    quiet()?;
    gates(pane)
}

/// Idle with an empty main prompt.
fn gates(pane: &dyn Pane) -> std::result::Result<(), &'static str> {
    match pane.agent_status() {
        Ok(Some(s)) if s == "idle" || s == "done" => {}
        Ok(Some(s)) if s == "working" => return Err("turn_active"),
        Ok(Some(s)) if s == "blocked" => return Err("approval_or_user_input_required"),
        Ok(_) => return Err("state_unverified"),
        Err(_) => return Err("TERMINAL_HOST_UNAVAILABLE"),
    }
    let rows = pane.screen().map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
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

fn commands(r: &Record) -> [Option<String>; 2] {
    [
        Some(format!("/model {}", r.requested_model)),
        target_effort(r).map(|e| format!("/effort {e}")),
    ]
}
/// A command of this operation left typed (unsubmitted) by a worker that died: exact match.
fn own_command(rows: &[Vec<Cell>], r: &Record) -> Option<String> {
    let Ok(View::Main(d)) = classify(rows, false) else {
        return None;
    };
    commands(r).into_iter().flatten().find(|c| *c == d)
}

pub(crate) async fn apply(
    paths: &Paths,
    r: &Record,
    pane: &dyn Pane,
    home: &Path,
) -> std::result::Result<Observed, Failure> {
    let hook = || crate::session_model::hook_record(home, &r.conversation_id);
    let observed = |h: &HookRecord| Observed::from_hook(h);
    let model_ok = |h: &HookRecord| {
        h.model_id
            .as_deref()
            .is_some_and(|m| crate::session_model::same_model(m, &r.requested_model))
    };
    // A command a dead worker typed but never submitted: clear exactly that.
    if let Ok(rows) = pane.screen()
        && let Some(c) = own_command(&rows, r)
    {
        withdraw(pane, &c).await.map_err(|why| (why, None))?;
    }
    // 1. Model (skipped when the session already reports it).
    if !hook().is_some_and(|h| model_ok(&h)) {
        step(paths, &r.id, "model_typed");
        run_command(
            paths,
            r,
            pane,
            home,
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
                pane,
                home,
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
    pane: &dyn Pane,
    home: &Path,
    command: &str,
    kind: Kind,
    done: &dyn Fn(&HookRecord) -> bool,
) -> std::result::Result<(), Failure> {
    let home_hook = || crate::session_model::hook_record(home, &r.conversation_id);
    let screen = |pane: &dyn Pane| {
        pane.screen()
            .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", None))
    };
    let rows = screen(pane)?;
    if let Some(d) = dialog(&rows) {
        // A dialog left open by a stopped worker: answer it and go on confirming.
        answer(pane, d, kind).await?;
    } else {
        match classify(&rows, true) {
            Ok(View::Main(d)) if d.is_empty() => {}
            Ok(View::Main(_)) => return Err(("TERMINAL_DRAFT_CHANGED", Some(rows))),
            Ok(_) => return Err(("TERMINAL_VIEW_NOT_MAIN", Some(rows))),
            Err(code) => return Err((code, Some(rows))),
        }
        pane.text(command)
            .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", None))?;
        let deadline = Instant::now() + crate::timing::scaled(TYPED_WAIT);
        let mut last;
        loop {
            tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
            last = screen(pane)?;
            if command_view(&last, command) {
                break;
            }
            if Instant::now() >= deadline {
                // Clear only what this worker typed; anything else is left untouched.
                if let Err(why) = withdraw(pane, command).await {
                    return Err((why, Some(last)));
                }
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
        pane.keys(&["enter"])
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
        last = screen(pane)?;
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
            answer(pane, d, kind).await?;
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

/// Remove `command` (or the prefix of it this worker managed to type) with backspaces, only
/// after verifying that the prompt holds exactly such a prefix. Anything else in the prompt
/// is not this worker's: nothing is cleared (`prompt_not_owned`).
pub(crate) async fn withdraw(
    pane: &dyn Pane,
    command: &str,
) -> std::result::Result<(), &'static str> {
    let rows = pane.screen().map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
    let typed = match classify(&rows, false) {
        Ok(View::Main(d)) => d,
        _ => return Err("prompt_not_owned"),
    };
    if typed.is_empty() {
        return Ok(());
    }
    if typed.contains('\n') || !command.starts_with(typed.as_str()) {
        return Err("prompt_not_owned");
    }
    let keys = vec!["backspace"; typed.chars().count()];
    pane.keys(&keys).map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
    let deadline = Instant::now() + crate::timing::scaled(TYPED_WAIT);
    loop {
        tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
        let rows = pane.screen().map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
        if matches!(classify(&rows, true), Ok(View::Main(ref d)) if d.is_empty()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("TERMINAL_CLEAR_UNVERIFIED");
        }
    }
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

/// The title row of a model or effort confirmation, if exactly one is on screen.
pub(crate) fn dialog_title(rows: &[Vec<Cell>]) -> Option<(usize, Kind)> {
    let titles: Vec<(usize, Kind)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match term::row_text(r).trim() {
            "Switch model?" => Some((i, Kind::Model)),
            "Change effort level?" => Some((i, Kind::Effort)),
            _ => None,
        })
        .collect();
    match titles.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// One option row: optional `❯` pointer, a number, `. `, then `Yes, …` or exactly `No` /
/// `No, …` (Claude Code 2.1.314: `❯ 1. Yes, switch to Opus 5.5`, `  2. No, go back`).
fn option_row(line: &str) -> Option<(bool, bool)> {
    let t = line.trim_start();
    let pointed = t.starts_with('❯');
    let rest = t.trim_start_matches('❯').trim_start();
    let (n, text) = rest.split_once(". ")?;
    if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let text = text.trim_end();
    if text.starts_with("Yes, ") {
        Some((true, pointed))
    } else if text == "No" || text.starts_with("No, ") {
        Some((false, pointed))
    } else {
        None
    }
}

/// A `Switch model?` / `Change effort level?` confirmation in the exact captured format:
/// one title row, then exactly one numbered `Yes, …` and one numbered `No…` option.
pub(crate) fn dialog(rows: &[Vec<Cell>]) -> Option<Dialog> {
    let (title, kind) = dialog_title(rows)?;
    let lines: Vec<String> = rows[title + 1..]
        .iter()
        .take(16)
        .map(|r| term::row_text(r))
        .collect();
    let options: Vec<(bool, bool)> = lines.iter().filter_map(|l| option_row(l)).collect();
    let yes: Vec<&(bool, bool)> = options.iter().filter(|o| o.0).collect();
    let no: Vec<&(bool, bool)> = options.iter().filter(|o| !o.0).collect();
    if yes.len() != 1 || no.len() != 1 {
        return None;
    }
    let pointer = match (yes[0].1, no[0].1) {
        (true, false) => Pointer::Yes,
        (false, true) => Pointer::No,
        _ => Pointer::None,
    };
    let hook_asked = lines.iter().any(|l| l.contains("PreModelSwitch hook"));
    Some(Dialog {
        kind,
        pointer,
        hook_asked,
    })
}

/// Answer a dialog: Yes (moving the pointer with up/down only), or No when a hook asked. Any
/// failure is returned with the dialog still open; the caller closes it ([`close_dialog`]).
async fn answer(pane: &dyn Pane, d: Dialog, kind: Kind) -> std::result::Result<(), Failure> {
    let reread = || async {
        tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
        pane.screen().ok().and_then(|rows| dialog(&rows))
    };
    if d.kind != kind {
        return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", pane.screen().ok()));
    }
    let key = |k: &str| {
        pane.keys(&[k])
            .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", None::<Vec<Vec<Cell>>>))
    };
    if d.hook_asked {
        return Err(("blocked_by_hook", pane.screen().ok()));
    }
    match d.pointer {
        Pointer::Yes => {}
        Pointer::No => {
            key("up")?;
            if reread().await.map(|d| d.pointer) != Some(Pointer::Yes) {
                return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", pane.screen().ok()));
            }
        }
        Pointer::None => return Err(("TERMINAL_OVERLAY_UNIDENTIFIED", pane.screen().ok())),
    }
    key("enter")?;
    Ok(())
}

/// Close a model or effort confirmation left on screen: choose its `No` option (moving the
/// pointer down when it is on Yes), or Esc when the options cannot be read; then verify that
/// the main prompt is back. `Err` when that cannot be confirmed.
pub(crate) async fn close_dialog(pane: &dyn Pane) -> std::result::Result<(), ()> {
    match pane.screen() {
        // No dialog of ours: nothing to close (the failure reason stands as it is).
        Ok(rows) if dialog_title(&rows).is_none() => return Ok(()),
        Ok(_) => {}
        Err(_) => return Err(()),
    }
    for _ in 0..3 {
        let Ok(rows) = pane.screen() else {
            return Err(());
        };
        if dialog_title(&rows).is_none() {
            break;
        }
        let sent = match dialog(&rows) {
            Some(d) if d.pointer == Pointer::No => pane.keys(&["enter"]),
            Some(d) if d.pointer == Pointer::Yes => {
                let moved = pane.keys(&["down"]);
                tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
                match (moved, pane.screen().ok().and_then(|r| dialog(&r))) {
                    (Ok(()), Some(d)) if d.pointer == Pointer::No => pane.keys(&["enter"]),
                    _ => pane.keys(&["esc"]),
                }
            }
            _ => pane.keys(&["esc"]),
        };
        if sent.is_err() {
            return Err(());
        }
        let deadline = Instant::now() + crate::timing::scaled(TYPED_WAIT);
        while Instant::now() < deadline {
            tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
            if pane.screen().is_ok_and(|r| dialog_title(&r).is_none()) {
                break;
            }
        }
    }
    match pane.screen() {
        Ok(rows)
            if dialog_title(&rows).is_none()
                && matches!(classify(&rows, false), Ok(View::Main(_))) =>
        {
            Ok(())
        }
        _ => Err(()),
    }
}
