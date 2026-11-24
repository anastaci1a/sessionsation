//! Claude side of `session compact` (contract 1.64.0, SM6): type `/compact` into the session's
//! verified HERDR pane at the next idle point and follow it until Claude reports the
//! compaction.
//!
//! Rules (the same as set-model's): terminal input must be on and the pane verified for this
//! exact process; the prompt must be empty twice, at least 1.5 s apart, with no terminal
//! delivery in flight (a draft is never cleared: the operation waits with `draft_present`);
//! only `/compact` is typed and it is read back before Enter (Claude may draw its command
//! menu above the box with `/compact` selected, which is accepted only when that entry is the
//! highlighted one); a command left typed is removed with backspaces only when the prompt holds
//! exactly what this pass typed. Enter is pressed once. A start that may have been submitted is
//! never typed again: the operation follows it and fails `compaction_not_observed` instead.
//!
//! Done when Claude reports a compaction at or after the start: its `PostCompact` hook event
//! (see [`crate::provider::events`]) or a `compact_boundary` record in the transcript.
use super::*;
use crate::messaging::claude_steer::terminal::{self as term, Cell, Host, View, classify};
// The waits and quiet window are set-model's: both type into the same Claude prompt.
use crate::messaging::model_change::claude::{
    DELIVERY_QUIET_SECONDS, Herdr, IDLE_GAP, Pane, SETTLE, TYPED_WAIT, command_view, gates,
    runtime_key,
};
use std::path::Path;
use std::time::{Duration, Instant};

pub const COMMAND: &str = "/compact";
/// A submitted compaction whose pane is idle again with no compaction reported after this long.
const IDLE_UNOBSERVED_SECONDS: i64 = 120;
/// Waiting attempts a typing mismatch may use before the operation fails.
const MAX_TYPING_ATTEMPTS: u32 = 3;

/// Admission for a Claude session: terminal input on, the exact process running in a verified
/// HERDR pane. Returns the runtime key and the account home.
pub(crate) fn admit(
    paths: &Paths,
    session: &crate::catalog::Session,
) -> anyhow::Result<(String, PathBuf)> {
    ensure!(term::enabled(paths), "{}", term::DISABLED);
    let account = crate::provider::claude::account(paths, &session.account)?;
    let o = crate::provider::claude::observe(
        &account.home,
        &session.conversation_id,
        session.attachment.as_ref(),
    );
    ensure!(o.availability == "running", "SESSION_NOT_RUNNING");
    let t = term::eligible(&o).map_err(|code| anyhow::anyhow!("{code}"))?;
    Ok((runtime_key(&t), account.home))
}

/// Re-observe the target: the same process and pane as admitted.
fn target(paths: &Paths, r: &Record) -> std::result::Result<(term::Terminal, PathBuf), String> {
    if !term::enabled(paths) {
        return Err("terminal_input_disabled".into());
    }
    let account = r
        .account
        .as_deref()
        .and_then(|a| crate::provider::claude::account(paths, a).ok())
        .ok_or("session_not_running")?;
    let session = crate::catalog::find(paths, &r.session_id).map_err(|_| "generation_changed")?;
    if session.conversation_id != r.conversation_id {
        return Err("generation_changed".into());
    }
    let o = crate::provider::claude::observe(
        &account.home,
        &r.conversation_id,
        session.attachment.as_ref(),
    );
    if o.availability != "running" {
        return Err("session_not_running".into());
    }
    let t = term::eligible(&o).map_err(str::to_owned)?;
    if runtime_key(&t) != r.runtime_generation {
        return Err("generation_changed".into());
    }
    Ok((t, account.home))
}

pub(super) async fn advance(paths: &Paths, id: &str) {
    let Ok(r) = read(paths, id) else { return };
    let now = crate::daemon::now();
    match r.phase.as_str() {
        "waiting" if now >= r.expires_at => finish(paths, id, &["waiting"], "expired", "expired"),
        "waiting" => start(paths, r).await,
        // Only a pass that died while typing leaves `starting`: never type again.
        "starting" => finish(paths, id, &["starting"], "failed", "start_interrupted"),
        "running" => follow(paths, r).await,
        _ => {}
    }
}

async fn start(paths: &Paths, r: Record) {
    if super::super::model_change::pending_before(paths, &r.conversation_id, (r.created_at, &r.id))
    {
        return waiting(paths, &r.id, "model_change_pending");
    }
    let t = match target(paths, &r) {
        Ok((t, _)) => t,
        Err(code) if code == "generation_changed" => {
            return finish(paths, &r.id, &["waiting"], "failed", "generation_changed");
        }
        Err(code) => {
            let code: &'static str = match code.as_str() {
                "terminal_input_disabled" => "native_sending_disabled",
                "session_not_running" => "session_not_running",
                _ => "state_unverified",
            };
            return waiting(paths, &r.id, code);
        }
    };
    // One terminal procedure per pane at a time (shared with deliveries and set-model).
    let steer = paths.root.join(crate::layout::CLAUDE_STEER);
    if private_dir(&steer).is_err() {
        return waiting(paths, &r.id, "state_unverified");
    }
    let key: String = t
        .target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let Ok(pane_lock) = lock_file(&steer.join(format!("terminal-{key}.lock"))) else {
        return waiting(paths, &r.id, "state_unverified");
    };
    if pane_lock.try_lock_exclusive().is_err() {
        return waiting(paths, &r.id, "session_busy");
    }
    let Ok(host) = Host::for_agent(&t, "claude") else {
        return waiting(paths, &r.id, "provider_unavailable");
    };
    procedure(paths, &r.id, &Herdr(host)).await;
    drop(pane_lock);
}

/// The one wait of this procedure (scaled in tests).
async fn pause(d: Duration) {
    tokio::time::sleep(crate::timing::scaled(d)).await;
}

/// Idle with an empty prompt twice, with no terminal delivery in flight or just done.
async fn idle_twice(
    paths: &Paths,
    r: &Record,
    pane: &(dyn Pane + Sync),
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
    pause(IDLE_GAP).await;
    quiet()?;
    gates(pane)
}

/// Everything after the target and pane lock are verified (testable with a fake pane).
pub(super) async fn procedure(paths: &Paths, id: &str, pane: &(dyn Pane + Sync)) {
    let Ok(r) = read(paths, id) else { return };
    if r.phase != "waiting" {
        return;
    }
    if let Err(why) = idle_twice(paths, &r, pane).await {
        return waiting(paths, id, why);
    }
    let claimed = update(paths, id, |x| {
        if x.phase != "waiting" || crate::daemon::now() >= x.expires_at {
            return false;
        }
        x.phase = "starting".into();
        x.reason = None;
        x.attempts += 1;
        true
    });
    let Ok(Some(claimed)) = claimed else { return };
    match submit(pane).await {
        Ok(()) | Err((_, true)) => {
            let _ = update(paths, id, |x| {
                if x.phase != "starting" {
                    return false;
                }
                x.phase = "running".into();
                x.started_at = Some(crate::daemon::now());
                true
            });
        }
        Err((why, false)) => {
            let retry = matches!(why, "TERMINAL_TYPED_TEXT_MISMATCH" | "draft_present")
                && claimed.attempts < MAX_TYPING_ATTEMPTS;
            let _ = update(paths, id, |x| {
                if x.phase != "starting" {
                    return false;
                }
                if retry {
                    x.phase = "waiting".into();
                    x.reason = Some(why.into());
                } else {
                    x.phase = "failed".into();
                    x.reason = Some(why.into());
                    x.finished_at = Some(crate::daemon::now());
                }
                true
            });
        }
    }
}

/// The command menu Claude draws directly above the input box: `(first row, top rule row)`,
/// empty when there is none (entries and their wrapped descriptions).
fn menu_rows(rows: &[Vec<Cell>]) -> Option<(usize, usize)> {
    let lines: Vec<String> = rows.iter().map(|r| term::row_text(r)).collect();
    let pointer = |l: &str| {
        let mut c = l.chars();
        c.next() == Some('❯') && matches!(c.next(), None | Some(' ' | '\u{a0}'))
    };
    let p = (1..lines.len())
        .rev()
        .find(|&i| pointer(&lines[i]) && term::is_rule(&rows[i - 1]))?;
    let mut first = p - 1;
    while first > 0 {
        let line = &lines[first - 1];
        let (indent, rest) = term::indent_of(line);
        if rest.is_empty() || !(term::menu_row(line) || indent >= 6) {
            break;
        }
        first -= 1;
    }
    Some((first, p - 1))
}
/// The screen with the command menu blanked.
fn without_menu(rows: &[Vec<Cell>]) -> Vec<Vec<Cell>> {
    let mut out = rows.to_vec();
    if let Some((first, rule)) = menu_rows(rows) {
        for row in out.iter_mut().take(rule).skip(first) {
            row.clear();
        }
    }
    out
}

/// The prompt holds exactly `/compact`: either with no menu, or with Claude's command menu
/// drawn above the box and `/compact` as its one highlighted entry.
pub(crate) fn compact_view(rows: &[Vec<Cell>]) -> bool {
    if command_view(rows, COMMAND) {
        return true;
    }
    let Some((first, rule)) = menu_rows(rows) else {
        return false;
    };
    let selected: Vec<String> = rows[first..rule]
        .iter()
        .map(|r| term::row_text(r))
        .filter(|l| {
            let (indent, rest) = term::indent_of(l);
            indent == 2 && rest.starts_with('❯')
        })
        .collect();
    let [selected] = selected.as_slice() else {
        return false;
    };
    let entry = selected
        .trim_start()
        .trim_start_matches('❯')
        .trim_start_matches([' ', '\u{a0}']);
    let Some(after) = entry.strip_prefix(COMMAND) else {
        return false;
    };
    (after.is_empty() || after.starts_with([' ', '\u{a0}']))
        && command_view(&without_menu(rows), COMMAND)
}

/// Type `/compact`, read it back, press Enter once and see the prompt let go of it.
/// `Err((why, submitted))`: `submitted` when Enter may have reached Claude.
async fn submit(pane: &(dyn Pane + Sync)) -> std::result::Result<(), (&'static str, bool)> {
    let screen = || {
        pane.screen()
            .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", false))
    };
    match classify(&screen()?, true) {
        Ok(View::Main(d)) if d.is_empty() => {}
        Ok(View::Main(_)) => return Err(("draft_present", false)),
        Ok(_) => return Err(("TERMINAL_VIEW_NOT_MAIN", false)),
        Err(code) => return Err((code, false)),
    }
    pane.text(COMMAND)
        .map_err(|_| ("TERMINAL_HOST_UNAVAILABLE", false))?;
    let deadline = Instant::now() + crate::timing::scaled(TYPED_WAIT);
    loop {
        pause(SETTLE).await;
        if compact_view(&screen()?) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(match clear_command(pane).await {
                Ok(()) => ("TERMINAL_TYPED_TEXT_MISMATCH", false),
                Err(why) => (why, false),
            });
        }
    }
    pane.keys(&["enter"])
        .map_err(|_| ("TERMINAL_SUBMIT_UNCERTAIN", true))?;
    let deadline = Instant::now() + crate::timing::scaled(TYPED_WAIT);
    loop {
        pause(SETTLE).await;
        let Ok(rows) = pane.screen() else {
            return Err(("TERMINAL_SUBMIT_UNCERTAIN", true));
        };
        if !compact_view(&rows) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Still typed: Enter did not run it. Clear exactly what was typed.
            return Err(match clear_command(pane).await {
                Ok(()) => ("compaction_not_started", false),
                Err(why) => (why, false),
            });
        }
    }
}

/// Remove what this pass typed: only when the prompt holds exactly a prefix of `/compact`
/// (menu or not), with that many backspaces, then verify the prompt is empty. Anything else
/// in the prompt is not this pass's (`prompt_not_owned`).
async fn clear_command(pane: &(dyn Pane + Sync)) -> std::result::Result<(), &'static str> {
    let rows = pane.screen().map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
    let typed = (1..=COMMAND.len())
        .rev()
        .map(|n| &COMMAND[..n])
        .find(|prefix| prefix_view(&rows, prefix));
    let Some(typed) = typed else {
        return match classify(&rows, true) {
            Ok(View::Main(d)) if d.is_empty() => Ok(()),
            _ => Err("prompt_not_owned"),
        };
    };
    let keys = vec!["backspace"; typed.chars().count()];
    pane.keys(&keys).map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
    let deadline = Instant::now() + crate::timing::scaled(TYPED_WAIT);
    loop {
        pause(SETTLE).await;
        let rows = pane.screen().map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
        if matches!(classify(&rows, true), Ok(View::Main(ref d)) if d.is_empty()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("TERMINAL_CLEAR_UNVERIFIED");
        }
    }
}
/// The prompt holds exactly `typed`, with or without a command menu above the box.
fn prefix_view(rows: &[Vec<Cell>], typed: &str) -> bool {
    command_view(rows, typed) || command_view(&without_menu(rows), typed)
}

/// Unix seconds of the newest compaction Claude reported for the conversation.
fn reported(paths: &Paths, r: &Record, home: Option<&Path>) -> Option<i64> {
    let hooked = crate::provider::events::tasks(paths)
        .get(&r.conversation_id)
        .and_then(|t| t.last_compacted_at);
    let transcript = home.and_then(|h| {
        crate::provider::compact::compacted_at(
            crate::provider::ProviderId::Claude,
            h,
            &r.conversation_id,
        )
    });
    hooked.max(transcript)
}

async fn follow(paths: &Paths, r: Record) {
    let _ = crate::provider::events::ingest(paths);
    let target = target(paths, &r);
    let home = target.as_ref().ok().map(|(_, h)| h.clone()).or_else(|| {
        r.account
            .as_deref()
            .and_then(|a| crate::provider::claude::account(paths, a).ok())
            .map(|a| a.home)
    });
    let idle = match &target {
        Ok((t, _)) => Host::for_agent(t, "claude")
            .ok()
            .and_then(|h| h.agent_status().ok().flatten())
            .map(|s| s == "idle" || s == "done"),
        Err(_) => None,
    };
    settle(
        paths,
        &r,
        reported(paths, &r, home.as_deref()),
        idle,
        target.err(),
    );
}

/// Decide a running Claude compaction from what was observed. Pure of provider I/O.
pub(super) fn settle(
    paths: &Paths,
    r: &Record,
    reported: Option<i64>,
    idle: Option<bool>,
    target_error: Option<String>,
) {
    let now = crate::daemon::now();
    let since = r.started_at.unwrap_or(r.updated_at);
    let from = &["running"];
    if reported.is_some_and(|at| at >= since - 1) {
        return finish(paths, &r.id, from, "complete", "compacted");
    }
    match target_error.as_deref() {
        Some("generation_changed") => {
            return finish(paths, &r.id, from, "failed", "generation_changed");
        }
        Some("session_not_running") => {
            return finish(paths, &r.id, from, "failed", "compaction_not_observed");
        }
        _ => {}
    }
    if now - since > RUN_LIMIT_SECONDS {
        return finish(paths, &r.id, from, "failed", "compaction_timeout");
    }
    if idle == Some(true) && now - since > IDLE_UNOBSERVED_SECONDS {
        finish(paths, &r.id, from, "failed", "compaction_not_observed");
    }
}

#[cfg(test)]
mod tests;
