//! Antigravity CLI 1.4.10 terminal input (contract revision 24): submit, queue
//! and steer for an interactive agy session that Sessionsation tracks and that
//! runs in a verified HERDR pane (HERDR agent kind `agy`). It reuses the Claude
//! terminal-input machinery ([`super::terminal`]): HERDR typed input without
//! bracketed paste, the draft round-trip, screen checks, journaling before
//! every write, transcript anchoring and no replay.
//!
//! What a key does depends on the session's state and on agy's
//! `queuedMessages` setting (`queue`, the default, or `send-immediately`):
//! - idle: Enter starts a user turn (`submit`; `queue`/`steer` on an idle
//!   session also become this, reported as `outcome:"submitted"`).
//! - busy: Enter follows the setting and alt+m ("send with opposite queued
//!   mode") does the other. `queue` presses the key that queues (the message
//!   runs when the turn ends); `steer` presses the key that sends immediately
//!   (agy hands it to the running turn: "Sending to agent..."). `submit` while
//!   busy presses Enter and reports which of the two the setting made it.
//!   The setting comes from an inline `--settings` JSON on the process command
//!   line, else the account's `settings.json`; when it cannot be determined,
//!   queue and steer are refused.
//!
//! Keys: text with `pane send-text`, newlines with ctrl+j, clearing with
//! alt+> (end of input) then ctrl+u (kill to line start) and backspace (join
//! the previous line), submit with enter or alt+m. Esc halts a running turn
//! and Ctrl+C / Ctrl+D arm agy's exit; Ctrl+K approves a subagent, up/down
//! edit queued messages or history: none of these is ever sent.
use super::{
    Record, Request, abandon_unsent, change, dir, finish, is_terminal, read,
    terminal::{
        self, CONFIRM_SECONDS, Cell, Host, SCREEN_WAIT, SETTLE, Terminal, columns, now_ms, reason,
        reject, row_text, squash, step, timestamp_ms,
    },
    worker_lock,
};
use crate::{
    catalog,
    paths::{Paths, lock_file, private_dir},
    provider::{ProviderId, antigravity, antigravity::QueueMode},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Read, Seek},
    path::Path,
    time::{Duration, Instant},
};

/// HERDR's agent kind for agy panes.
pub const AGENT: &str = "agy";
pub const PROVIDER: &str = "antigravity";
pub const LABEL: &str = "Typed into the session's HERDR terminal as the user's own input: agy records a user turn when idle; mid-turn the message is queued or sent to the running turn, per its queuedMessages setting and the key used";
pub const LABEL_QUEUE: &str =
    "Queued in agy's own message queue as the user's input: run when the current turn ends";
pub const LABEL_STEER: &str = "Steer";
pub const STEER_CAVEAT: &str = "agy sends the message to the running turn immediately (\"Sending to agent...\"); the model sees it at its next step and may answer within the same turn";
/// agy's input box grows with the draft; typed text must stay verifiable on screen.
pub const MAX_TEXT: usize = 1000;
pub const MAX_LINES: usize = 8;
const DRAFT_ROWS: usize = 9;
const MAX_SCAN: u64 = 16 * 1024 * 1024;
pub const QUEUE_MODE_UNKNOWN: &str = "ANTIGRAVITY_QUEUE_MODE_UNKNOWN: the session's queuedMessages setting cannot be determined (a --settings file, or an unreadable settings.json), so the key that queues or sends immediately is unknown";

pub const REASONS: &[&str] = &[
    "TERMINAL_NOT_ATTACHED",
    "TERMINAL_INPUT_DISABLED",
    "TERMINAL_INTERRUPT_UNSUPPORTED",
    "TERMINAL_PAYLOAD_UNSUPPORTED",
    "TERMINAL_TEXT_TOO_LONG",
    "TERMINAL_HOST_UNAVAILABLE",
    "TERMINAL_AGENT_UNKNOWN",
    "TERMINAL_AGENT_BLOCKED",
    "TERMINAL_VIEW_UNDETERMINED",
    "TERMINAL_OVERLAY_UNIDENTIFIED",
    "TERMINAL_EXIT_ARMED",
    "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
    "TERMINAL_DRAFT_UNRESTORABLE",
    "TERMINAL_CLEAR_UNVERIFIED",
    "TERMINAL_TYPED_TEXT_MISMATCH",
    "TERMINAL_HOST_FAILED_BEFORE_SUBMIT",
    "TERMINAL_WORKER_EXITED_BEFORE_SUBMIT",
    "TERMINAL_SUBMIT_UNCERTAIN",
    "TERMINAL_INPUT_UNCONFIRMED",
    "ANTIGRAVITY_QUEUE_MODE_UNKNOWN",
];

/// Antigravity-specific delivery facts kept in the journal and the receipt.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    /// The session's queuedMessages (`queue` | `send-immediately`; None: unknown).
    pub queue_mode: Option<String>,
    pub queue_mode_source: Option<String>,
    /// The screen or HERDR showed a running turn just before the key.
    pub busy_at_submit: Option<bool>,
    /// `enter` | `alt+m`
    pub key: Option<String>,
    /// `user_turn` | `native_queue` | `send_immediately`
    pub expected: Option<String>,
    /// `queued` | `sending`: what the screen showed right after a busy key.
    pub screen_evidence: Option<String>,
    /// `submitted` | `queued` | `sent_immediately`
    pub outcome: Option<String>,
}

// ---------------------------------------------------------------- payload

/// The text as typed: CRLF normalized and trimmed. agy has no verified escape
/// for a leading `/` (slash menu), `?` (shortcuts) or `!`, so those are refused.
pub fn deliverable(text: &str) -> Result<(String, bool, bool)> {
    let normalized = text.replace("\r\n", "\n");
    ensure!(
        !normalized
            .chars()
            .any(|c| c != '\n' && (c.is_control() || c == '\u{7f}')),
        "TERMINAL_PAYLOAD_UNSUPPORTED: tabs and control characters cannot be typed verbatim"
    );
    let trimmed = normalized.trim();
    ensure!(!trimmed.is_empty(), "INVALID_REQUEST: empty text");
    ensure!(
        trimmed.len() <= MAX_TEXT && trimmed.lines().count() <= MAX_LINES,
        "TERMINAL_TEXT_TOO_LONG: terminal input into agy takes at most {MAX_TEXT} bytes and {MAX_LINES} lines"
    );
    ensure!(
        !trimmed.starts_with(['/', '?', '!']),
        "TERMINAL_PAYLOAD_UNSUPPORTED: agy treats a leading / ? or ! as a command, its shortcuts or a mode; rephrase the start of the text"
    );
    ensure!(
        !trimmed.ends_with('\\'),
        "TERMINAL_PAYLOAD_UNSUPPORTED: a trailing backslash turns Enter into a newline"
    );
    let last = trimmed.split_whitespace().last().unwrap_or_default();
    ensure!(
        !last.starts_with('@') && !last.starts_with('/'),
        "TERMINAL_PAYLOAD_UNSUPPORTED: text ending in an @ or / token would submit a completion; add punctuation after it"
    );
    Ok((
        trimmed.to_owned(),
        false,
        normalized.len() != text.len() || trimmed != normalized,
    ))
}

// ---------------------------------------------------------------- screen

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum View {
    /// The input prompt with no overlay; the draft (empty when none) and
    /// whether a running turn is shown.
    Main { draft: String, busy: bool },
}

/// A border of agy's input box: a full-width horizontal rule. Rules between
/// earlier turns are shorter.
fn is_border(row: &[Cell]) -> bool {
    let t = row_text(row);
    let t = t.trim_end();
    let n = t.chars().count();
    n >= 8 && t.chars().all(|c| c == '─') && n + 2 >= row.len()
}
fn visible(c: &Cell) -> bool {
    c.ch != ' ' && c.ch != '\u{a0}'
}
fn is_spinner(c: char) -> bool {
    ('\u{2801}'..='\u{28FF}').contains(&c)
}
/// The footer's left text (before the right-aligned model label), or None when
/// it starts far right (only the model label).
fn footer_left(line: &str) -> Option<String> {
    let lead = line.len() - line.trim_start().len();
    if lead > 8 {
        return None;
    }
    let t = line.trim_start();
    let left = t.split("   ").next().unwrap_or("").trim();
    (!left.is_empty()).then(|| left.to_owned())
}

/// Classify an agy screen. `strict` reads a user draft (any doubt refuses);
/// not strict reads back text the worker typed (compared without whitespace).
pub(crate) fn classify(rows: &[Vec<Cell>], strict: bool) -> Result<View, &'static str> {
    let lines: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    let mismatch = if strict {
        "TERMINAL_DRAFT_UNRECONSTRUCTABLE"
    } else {
        "TERMINAL_TYPED_TEXT_MISMATCH"
    };
    let Some(bottom) = rows.iter().rposition(|r| is_border(r)) else {
        return Err("TERMINAL_VIEW_UNDETERMINED");
    };
    let Some(top) = (bottom.saturating_sub(DRAFT_ROWS + 1)..bottom)
        .rev()
        .find(|&i| is_border(&rows[i]))
    else {
        return Err("TERMINAL_VIEW_UNDETERMINED");
    };
    let p = top + 1;
    if p >= bottom {
        return Err("TERMINAL_VIEW_UNDETERMINED");
    }
    let prompt = lines[p].trim_end();
    if !(prompt == ">" || prompt.starts_with("> ")) {
        return Err("TERMINAL_VIEW_UNDETERMINED");
    }
    // Below the box only the footer row: menus, panels and pickers add rows.
    let footer: Vec<&String> = lines[bottom + 1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if footer.len() > 1 {
        return Err("TERMINAL_OVERLAY_UNIDENTIFIED");
    }
    let left = footer.first().and_then(|l| footer_left(l));
    let footer_busy = match left.as_deref() {
        None | Some("? for shortcuts") => false,
        Some("esc to cancel") | Some("Press up to edit queued messages") => true,
        Some(l) if l.starts_with("press ctrl+") && l.contains("again to exit") => {
            return Err("TERMINAL_EXIT_ARMED");
        }
        Some(_) => return Err("TERMINAL_OVERLAY_UNIDENTIFIED"),
    };
    // A running turn shows a braille spinner row above the box.
    let above = &lines[top.saturating_sub(12)..top];
    let spinning = above.iter().any(|l| {
        let mut c = l.chars();
        c.next().is_some_and(is_spinner) && c.next() == Some(' ')
    });
    let busy = footer_busy || spinning;
    let width = rows[top].len();
    let mut content: Vec<&[Cell]> = vec![];
    for (i, row) in rows.iter().enumerate().take(bottom).skip(p) {
        if i > p && row.iter().take(2).any(visible) {
            return Err(mismatch);
        }
        content.push(row.get(2..).unwrap_or(&[]));
    }
    let as_text = |r: &[Cell]| -> String {
        r.iter()
            .map(|c| if c.ch == '\u{a0}' { ' ' } else { c.ch })
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let draft = if content.iter().all(|r| !r.iter().any(visible)) {
        if content.len() > 1 {
            return Err(mismatch);
        }
        String::new()
    } else {
        if strict {
            // One text colour only: another is ghost text or a highlighted token.
            let mut colors: Vec<u32> = content
                .iter()
                .flat_map(|r| r.iter())
                .filter(|c| visible(c) && !c.style.inverse)
                .map(|c| c.style.color)
                .collect();
            colors.sort_unstable();
            colors.dedup();
            if colors.len() > 1 || content.iter().flat_map(|r| r.iter()).any(|c| c.style.dim) {
                return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
            }
            // A row that could not have taken the next row's first word may be a
            // soft wrap, not a newline.
            let room = width.saturating_sub(2 + 3);
            for (i, row) in content.iter().enumerate() {
                let used = columns(
                    as_text(row)
                        .chars()
                        .map(|ch| Cell {
                            ch,
                            style: Default::default(),
                        })
                        .collect::<Vec<_>>()
                        .as_slice(),
                );
                if used >= room {
                    return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
                }
                if let Some(next) = content.get(i + 1) {
                    let next = as_text(next);
                    let word = next.trim_start().split(' ').next().unwrap_or("");
                    let lead = next.len() - next.trim_start().len();
                    let w: usize = word.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
                    if used + 1 + lead + w >= room {
                        return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
                    }
                }
            }
        }
        let draft = content
            .iter()
            .map(|r| as_text(r))
            .collect::<Vec<_>>()
            .join("\n");
        if strict && (draft.contains("[Pasted") || draft.contains("[Image")) {
            return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
        }
        draft
    };
    Ok(View::Main { draft, busy })
}
fn is_main(v: &Result<View, &'static str>, f: impl Fn(&str) -> bool) -> bool {
    matches!(v, Ok(View::Main { draft, .. }) if f(draft))
}

async fn until(
    host: &Host,
    strict: bool,
    want: impl Fn(&Result<View, &'static str>) -> bool,
) -> Result<Result<View, &'static str>> {
    let deadline = Instant::now() + SCREEN_WAIT;
    loop {
        tokio::time::sleep(SETTLE).await;
        let v = classify(&host.screen()?, strict);
        if want(&v) || Instant::now() >= deadline {
            return Ok(v);
        }
    }
}
/// Clear the input: alt+> (end of input), then per round ctrl+u (kill to the
/// line start) and backspace (join the previous line). None of these keys does
/// anything else in agy's prompt.
async fn clear(host: &Host, text: &str) -> Result<()> {
    let rows = text
        .split('\n')
        .map(|l| l.chars().count() / 30 + 1)
        .sum::<usize>();
    host.keys(&["alt+>"])?;
    for _ in 0..(rows + 2).min(100) {
        host.keys(&["ctrl+u", "backspace"])?;
    }
    Ok(())
}

// ---------------------------------------------------------------- admission

/// The session's queuedMessages (inline --settings on its command line first).
fn queue_mode_of(home: &Path, pid: Option<u32>) -> (Option<QueueMode>, &'static str) {
    let args = pid.and_then(antigravity::process_arguments);
    antigravity::queue_mode(home, args.as_deref())
}
/// The key for a mode on a busy session, or the refusal.
fn busy_key(
    mode: &str,
    q: Option<QueueMode>,
) -> Result<(&'static str, &'static str), &'static str> {
    match (mode, q) {
        ("queue", Some(QueueMode::Queue)) | ("steer", Some(QueueMode::SendImmediately)) => Ok((
            "enter",
            if mode == "queue" {
                "native_queue"
            } else {
                "send_immediately"
            },
        )),
        ("queue", Some(QueueMode::SendImmediately)) => Ok(("alt+m", "native_queue")),
        ("steer", Some(QueueMode::Queue)) => Ok(("alt+m", "send_immediately")),
        ("queue" | "steer", None) => Err(QUEUE_MODE_UNKNOWN),
        (_, Some(QueueMode::Queue)) => Ok(("enter", "native_queue")),
        (_, Some(QueueMode::SendImmediately)) => Ok(("enter", "send_immediately")),
        (_, None) => Ok(("enter", "unknown")),
    }
}
fn code(s: &str) -> &str {
    s.split(':').next().unwrap_or(s)
}

/// Admission of an Antigravity submit/queue/steer: the terminal target and facts.
pub(super) fn admit(
    request: &Request,
    session: &catalog::Session,
    home: &Path,
) -> Result<Terminal> {
    ensure!(
        matches!(
            request.delivery.as_deref(),
            None | Some("auto" | "terminal_input")
        ),
        "DELIVERY_UNAVAILABLE: Antigravity sessions take terminal_input only"
    );
    ensure!(
        !request.steering.as_ref().is_some_and(|s| s.interrupt),
        "TERMINAL_INTERRUPT_UNSUPPORTED: terminal input never sends Esc or Ctrl+C; omit steering.interrupt"
    );
    let o = antigravity::observe(home, &session.conversation_id, session.attachment.as_ref());
    ensure!(
        o.availability == "running",
        "SESSION_NOT_RUNNING: submit/queue/steer need a live Antigravity session in a HERDR pane; use mode resume (headless) for a stopped session"
    );
    let t = terminal::eligible(&o).map_err(|code| {
        anyhow::anyhow!(
            "{code}: terminal input needs a running session attached to a verified HERDR pane"
        )
    })?;
    let (q, source) = queue_mode_of(home, o.pid);
    if matches!(request.mode.as_str(), "queue" | "steer") && q.is_none() {
        anyhow::bail!("{QUEUE_MODE_UNKNOWN}");
    }
    let (text, escaped, normalized) = deliverable(&request.text)?;
    Ok(Terminal {
        transcript: o.transcript.clone(),
        delivered_text: text,
        escaped_prefix: escaped,
        normalized,
        antigravity: Some(Facts {
            queue_mode: q.map(|q| q.as_str().into()),
            queue_mode_source: Some(source.into()),
            ..Default::default()
        }),
        ..t
    })
}

// ---------------------------------------------------------------- worker

fn recheck(paths: &Paths, r: &Record) -> Result<(Terminal, std::path::PathBuf)> {
    ensure!(terminal::enabled(paths), "{}", terminal::DISABLED);
    let account =
        crate::messaging::headless::usable_account(paths, ProviderId::Antigravity, &r.account)?;
    let session = catalog::find(paths, &r.session_id)?;
    ensure!(session.conversation_id == r.conversation_id, "STALE_TARGET");
    let recorded = r.terminal.clone().context("STALE_TARGET")?;
    let o = antigravity::observe(
        &account.home,
        &r.conversation_id,
        session.attachment.as_ref(),
    );
    let now = terminal::eligible(&o).map_err(|code| anyhow::anyhow!("{code}"))?;
    ensure!(
        now.pid == recorded.pid
            && now.process_start == recorded.process_start
            && now.target == recorded.target,
        "STALE_TARGET: the attached agy process or pane changed"
    );
    Ok((
        Terminal {
            transcript: now.transcript.or(recorded.transcript.clone()),
            ..recorded
        },
        account.home,
    ))
}
fn set_facts(paths: &Paths, id: &str, f: impl FnOnce(&mut Facts)) -> Result<()> {
    change(paths, id, |r| {
        if let Some(t) = r.terminal.as_mut() {
            f(t.antigravity.get_or_insert_with(Facts::default));
        }
        Ok(())
    })
}

pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = lock_file(&worker_lock(paths, id)?)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let r = read(paths, id)?;
    if r.phase != "admitted" || r.kind != "terminal" || !is_antigravity(&r) {
        return Ok(());
    }
    if r.request.expires_at <= crate::daemon::now() {
        return change(paths, id, |r| {
            finish(r, "expired", Some("EXPIRED_BEFORE_DELIVERY"));
            Ok(())
        });
    }
    let (t, home) = match recheck(paths, &r) {
        Ok(x) => x,
        Err(e) => return abandon_unsent(paths, id, &reason(&e)),
    };
    let (q, source) = queue_mode_of(&home, Some(t.pid));
    if matches!(r.request.mode.as_str(), "queue" | "steer") && q.is_none() {
        return abandon_unsent(paths, id, code(QUEUE_MODE_UNKNOWN));
    }
    private_dir(&dir(paths))?;
    let key: String = t
        .target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let pane_lock = lock_file(&dir(paths).join(format!("terminal-{key}.lock")))?;
    pane_lock.lock_exclusive()?;
    change(paths, id, |r| {
        let mut t = t.clone();
        let facts = t.antigravity.get_or_insert_with(Facts::default);
        facts.queue_mode = q.map(|q| q.as_str().into());
        facts.queue_mode_source = Some(source.into());
        r.terminal = Some(t);
        Ok(())
    })?;
    let host = Host::for_agent(&t, AGENT)?;
    let outcome = deliver(paths, id, &host, &t, q).await;
    if let Err(e) = outcome {
        change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut()
                && t.draft.as_deref().is_some_and(|d| !d.is_empty())
                && t.draft_restored != Some(true)
            {
                t.draft_restored = Some(false);
                t.restore_error
                    .get_or_insert_with(|| "host_failed_draft_kept_in_journal".into());
            }
            if is_terminal(r) {
                return Ok(());
            }
            if r.facts.written {
                r.observation_error = Some(format!("worker_error: {}", reason(&e)));
            } else if r.phase == "admitted" {
                finish(r, "rejected", Some(&reason(&e)));
            } else {
                finish(r, "rejected", Some("TERMINAL_HOST_FAILED_BEFORE_SUBMIT"));
            }
            Ok(())
        })?;
    }
    drop(pane_lock);
    observe_loop(paths, id, &host).await
}

fn blocked(host: &Host) -> bool {
    matches!(host.agent_status(), Ok(Some(s)) if s == "blocked")
}
fn working(host: &Host) -> bool {
    matches!(host.agent_status(), Ok(Some(s)) if s == "working")
}

async fn withdraw(
    paths: &Paths,
    id: &str,
    host: &Host,
    typed: &str,
    draft: &str,
    code: &str,
) -> Result<()> {
    step(paths, id, "withdraw", |_| {})?;
    clear(host, typed).await?;
    let cleared = until(host, true, |v| is_main(v, str::is_empty)).await?;
    if is_main(&cleared, str::is_empty) {
        restore(paths, id, host, draft).await?;
    } else {
        change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.draft_restored = Some(false);
                t.restore_error = Some("withdraw_unverified_draft_kept_in_journal".into());
            }
            Ok(())
        })?;
    }
    reject(paths, id, code)
}

async fn deliver(
    paths: &Paths,
    id: &str,
    host: &Host,
    t: &Terminal,
    q: Option<QueueMode>,
) -> Result<()> {
    let mode = read(paths, id)?.request.mode;
    // 1. HERDR agent state (read-only).
    match host.agent_status() {
        Ok(Some(s)) if s == "blocked" => return reject(paths, id, "TERMINAL_AGENT_BLOCKED"),
        Ok(Some(_)) => {}
        Ok(None) => return reject(paths, id, "TERMINAL_AGENT_UNKNOWN"),
        Err(_) => return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE"),
    }
    // 2. View: the input prompt with no menu, panel or dialog.
    let Ok(screen) = host.screen() else {
        return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE");
    };
    let (draft, busy_screen) = match classify(&screen, true) {
        Ok(View::Main { draft, busy }) => (draft, busy),
        Err(code) => return reject(paths, id, code),
    };
    let last = draft.split_whitespace().last().unwrap_or_default();
    if draft.starts_with(['?', '!', '/']) || last.starts_with(['@', '/']) || draft.contains('\t') {
        // Typing it back would open a menu, the shortcuts or a completion.
        return reject(paths, id, "TERMINAL_DRAFT_UNRESTORABLE");
    }
    let busy = busy_screen || working(host);
    if busy && let Err(refusal) = busy_key(&mode, q) {
        return reject(paths, id, code(refusal));
    }
    if blocked(host) {
        return reject(paths, id, "TERMINAL_AGENT_BLOCKED");
    }
    // 3. Journal the draft, then clear.
    step(paths, id, "clear", |r| {
        r.phase = "dispatching".into();
        r.dispatched_at = Some(crate::daemon::now());
        if let Some(t) = r.terminal.as_mut() {
            t.draft = Some(draft.clone());
            t.draft_chars = Some(draft.chars().count());
        }
    })?;
    if !draft.is_empty() {
        clear(host, &draft).await?;
        let v = until(host, true, |v| is_main(v, str::is_empty)).await?;
        if !is_main(&v, str::is_empty) {
            return change(paths, id, |r| {
                if let Some(t) = r.terminal.as_mut() {
                    t.draft_restored = Some(false);
                    t.restore_error = Some("clear_unverified_draft_kept_in_journal".into());
                }
                finish(r, "rejected", Some("TERMINAL_CLEAR_UNVERIFIED"));
                Ok(())
            });
        }
    }
    // 4. Type the message and check it on screen.
    step(paths, id, "type", |_| {})?;
    host.type_text(&t.delivered_text).await?;
    let want = squash(&t.delivered_text);
    let typed = until(host, false, |v| is_main(v, |d| squash(d) == want)).await?;
    if !is_main(&typed, |d| squash(d) == want) || blocked(host) {
        let code = if blocked(host) {
            "TERMINAL_AGENT_BLOCKED"
        } else {
            "TERMINAL_TYPED_TEXT_MISMATCH"
        };
        return withdraw(paths, id, host, &t.delivered_text, &draft, code).await;
    }
    // 5. The key now: idle Enter is a user turn; busy follows the setting.
    let busy = matches!(typed, Ok(View::Main { busy: true, .. })) || working(host);
    let (key, expected) = if busy {
        match busy_key(&mode, q) {
            Ok(k) => k,
            Err(refusal) => {
                return withdraw(paths, id, host, &t.delivered_text, &draft, code(refusal)).await;
            }
        }
    } else {
        ("enter", "user_turn")
    };
    let anchor = antigravity_log(paths, &read(paths, id)?)
        .and_then(|p| fs::metadata(p).ok())
        .map(|m| m.len());
    let at = now_ms() - 2000;
    step(paths, id, "submit", |r| {
        if let Some(t) = r.terminal.as_mut() {
            t.anchor_offset = anchor;
            t.submitted_at_ms = Some(at);
            let f = t.antigravity.get_or_insert_with(Facts::default);
            f.busy_at_submit = Some(busy);
            f.key = Some(key.into());
            f.expected = Some(expected.into());
        }
    })?;
    let sent = host.keys(&[key]);
    change(paths, id, |r| {
        if sent.is_ok() {
            r.phase = "written".into();
            r.facts.written = true;
        } else {
            finish(r, "delivery_unknown", Some("TERMINAL_SUBMIT_UNCERTAIN"));
        }
        Ok(())
    })?;
    if sent.is_err() {
        return Ok(());
    }
    // 6. The input must empty; a busy key shows agy's queue row or "Sending".
    let empty = until(host, true, |v| is_main(v, str::is_empty)).await?;
    if busy {
        let lines: Vec<String> = host
            .screen()
            .map(|s| s.iter().map(|r| row_text(r)).collect())
            .unwrap_or_default();
        if let Some(e) = screen_evidence(&lines, &t.delivered_text) {
            set_facts(paths, id, |f| f.screen_evidence = Some(e.into()))?;
            change(paths, id, |r| {
                if !is_terminal(r) && r.state == "pending_local" {
                    r.state = "native_accepted".into();
                }
                Ok(())
            })?;
        }
    }
    // 7. Put the draft back, unsubmitted.
    if !draft.is_empty() {
        if is_main(&empty, str::is_empty) {
            restore(paths, id, host, &draft).await?;
        } else {
            change(paths, id, |r| {
                if let Some(t) = r.terminal.as_mut() {
                    t.draft_restored = Some(false);
                    t.restore_error = Some("input_not_empty_after_submit".into());
                }
                Ok(())
            })?;
        }
    } else {
        change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.draft = None;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// `queued`: agy listed the message as `▸ text (alt+m to send immediately)`;
/// `sending`: `▸ text` followed by `⎿  Sending to agent...`.
pub(crate) fn screen_evidence(lines: &[String], text: &str) -> Option<&'static str> {
    let head: String = squash(text).chars().take(24).collect();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        let Some(rest) = t.strip_prefix('▸') else {
            continue;
        };
        if !squash(rest).contains(&head) {
            continue;
        }
        if t.contains("(alt+m to send immediately)") {
            return Some("queued");
        }
        if lines[i + 1..]
            .iter()
            .take(2)
            .any(|n| n.contains("Sending to agent"))
        {
            return Some("sending");
        }
    }
    None
}

async fn restore(paths: &Paths, id: &str, host: &Host, draft: &str) -> Result<()> {
    if draft.is_empty() {
        return change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.draft = None;
                t.draft_restored = Some(true);
            }
            Ok(())
        });
    }
    step(paths, id, "restore", |_| {})?;
    let typed = host.type_text(draft).await;
    let v = until(host, true, |v| is_main(v, |d| d == draft)).await;
    let ok = typed.is_ok() && matches!(&v, Ok(v) if is_main(v, |d| d == draft));
    change(paths, id, |r| {
        if let Some(t) = r.terminal.as_mut() {
            t.draft_restored = Some(ok);
            if ok {
                t.draft = None;
            } else {
                t.restore_error = Some(match (&typed, &v) {
                    (Err(_), _) => "restore_keys_failed".into(),
                    (_, Ok(Err(code))) => format!("restore_unverified:{code}"),
                    _ => "restore_unverified".into(),
                });
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------- transcript

pub(super) fn is_antigravity(r: &Record) -> bool {
    r.provider.as_deref() == Some(PROVIDER)
}
/// agy's transcript log of the record's conversation.
fn antigravity_log(paths: &Paths, r: &Record) -> Option<std::path::PathBuf> {
    let account = antigravity::account(paths, &r.account).ok()?;
    Some(antigravity::transcript_log(
        &account.home,
        &r.conversation_id,
    ))
}
fn spaced(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// The user's text inside agy's `<USER_REQUEST>` wrapper.
fn user_request(content: &str) -> Option<&str> {
    let start = content.find("<USER_REQUEST>")? + "<USER_REQUEST>".len();
    let end = content[start..].find("</USER_REQUEST>")? + start;
    Some(content[start..end].trim())
}
fn user_input(v: &Value) -> bool {
    v["type"] == "USER_INPUT" && v["source"] == "USER_EXPLICIT"
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Found {
    /// The anchoring step's `step_index`.
    pub id: Option<String>,
    pub exact: bool,
    pub reply: Vec<String>,
    pub turn_ended: bool,
}

/// Find the first explicit user input at or after `since_ms` (and after
/// `offset`) whose request contains the typed text, then collect agy's
/// responses until the next explicit user input.
pub(crate) fn find(path: &Path, offset: u64, text: &str, since_ms: i64) -> Result<Found> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(std::io::SeekFrom::Start(if offset <= len {
        offset
    } else {
        0
    }))?;
    let reader = std::io::BufReader::new(f.take(MAX_SCAN));
    let mut out = Found::default();
    let typed = spaced(text.trim());
    for line in reader.split(b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(&line?) else {
            continue;
        };
        let content = v["content"].as_str().unwrap_or_default();
        if out.id.is_none() {
            let fresh = v["created_at"]
                .as_str()
                .and_then(timestamp_ms)
                .is_some_and(|t| t >= since_ms);
            if !user_input(&v) || !fresh {
                continue;
            }
            let Some(request) = user_request(content) else {
                continue;
            };
            if !spaced(request).contains(&typed) {
                continue;
            }
            out.exact = spaced(request) == typed;
            out.id = v["step_index"].as_u64().map(|i| i.to_string());
            continue;
        }
        if user_input(&v) {
            out.turn_ended = true;
            break;
        }
        if v["type"] == "PLANNER_RESPONSE" && !content.trim().is_empty() {
            out.reply.push(content.to_owned());
        }
    }
    Ok(out)
}

/// Apply transcript evidence to a written agy terminal record (never regressed).
pub(crate) fn observe(paths: &Paths, id: &str, idle: bool) -> Result<()> {
    let r = read(paths, id)?;
    if r.terminal.is_none() || is_terminal(&r) || !r.facts.written {
        return Ok(());
    }
    let t = r.terminal.clone().unwrap_or_default();
    let Some(path) = antigravity_log(paths, &r).filter(|p| p.is_file()) else {
        return change(paths, id, |r| {
            r.observation_error = Some("transcript_unavailable".into());
            Ok(())
        });
    };
    let found = find(
        &path,
        t.anchor_offset.unwrap_or(0),
        &t.delivered_text,
        t.submitted_at_ms.unwrap_or(0),
    );
    change(paths, id, |r| {
        if is_terminal(r) {
            return Ok(());
        }
        let Ok(found) = found else {
            r.observation_error = Some("transcript_unreadable".into());
            return Ok(());
        };
        let Some(t) = r.terminal.as_mut() else {
            return Ok(());
        };
        t.transcript = Some(path.clone());
        let Some(step) = found.id else {
            r.observation_error = Some("terminal_input_not_found".into());
            return Ok(());
        };
        r.observation_error = None;
        let facts = t.antigravity.get_or_insert_with(Facts::default);
        let outcome = match (facts.busy_at_submit, facts.expected.as_deref()) {
            (Some(true), Some("native_queue")) => "queued",
            (Some(true), Some("send_immediately")) => "sent_immediately",
            _ => "submitted",
        };
        facts.outcome = Some(outcome.into());
        t.input_uuid = Some(step);
        t.input_record = Some("user".into());
        t.turn_ended |= found.turn_ended;
        let ended = t.turn_ended || (idle && !found.reply.is_empty());
        r.facts.input_recorded = true;
        r.facts.folded = outcome == "sent_immediately";
        r.facts.shared_with_turn = outcome == "sent_immediately" || !found.exact;
        r.evidence_at = Some(crate::daemon::now());
        r.reply_text.clear();
        r.reply_truncated = false;
        for text in &found.reply {
            super::append_reply(r, text);
        }
        if r.started_at.is_none() {
            r.started_at = Some(crate::daemon::now());
        }
        if ended {
            if r.reply_text.is_empty() {
                finish(r, "completed_without_reply", None);
            } else {
                finish(r, "completed_with_reply", None);
            }
        } else {
            r.state = "execution_started".into();
        }
        Ok(())
    })
}

/// A queued message is recorded only when the turn reaches it, so it may wait
/// the whole request timeout; anything else 60 seconds.
fn confirm_window(r: &Record) -> Duration {
    let timeout = r
        .request
        .timeout_seconds
        .unwrap_or(super::DEFAULT_TIMEOUT_SECONDS);
    let pending = r
        .terminal
        .as_ref()
        .and_then(|t| t.antigravity.as_ref())
        .is_some_and(|a| a.busy_at_submit == Some(true) && a.screen_evidence.is_some());
    Duration::from_secs(if pending {
        timeout
    } else {
        timeout.min(CONFIRM_SECONDS)
    })
}

async fn observe_loop(paths: &Paths, id: &str, host: &Host) -> Result<()> {
    let r = read(paths, id)?;
    if is_terminal(&r) || !r.facts.written {
        return Ok(());
    }
    let timeout = r
        .request
        .timeout_seconds
        .unwrap_or(super::DEFAULT_TIMEOUT_SECONDS);
    let start = Instant::now();
    let confirm = confirm_window(&r);
    let total = Duration::from_secs(timeout);
    let mut worked = false;
    let mut idle_since: Option<Instant> = None;
    loop {
        let status = host.agent_status().ok().flatten();
        worked |= matches!(status.as_deref(), Some("working" | "blocked"));
        let idle = matches!(status.as_deref(), Some("idle" | "done"));
        if !idle {
            idle_since = None;
        } else if idle_since.is_none() {
            idle_since = Some(Instant::now());
        }
        let settled = worked && idle_since.is_some_and(|s| s.elapsed() >= Duration::from_secs(2));
        observe(paths, id, settled)?;
        let r = read(paths, id)?;
        if is_terminal(&r) {
            return Ok(());
        }
        let anchored = r.terminal.as_ref().is_some_and(|t| t.input_uuid.is_some());
        if !anchored && start.elapsed() >= confirm {
            return change(paths, id, |r| {
                finish(r, "delivery_unknown", Some("TERMINAL_INPUT_UNCONFIRMED"));
                Ok(())
            });
        }
        if anchored && settled {
            return change(paths, id, |r| {
                if let Some(t) = r.terminal.as_mut() {
                    t.turn_ended = true;
                }
                if r.reply_text.is_empty() {
                    finish(r, "completed_without_reply", None);
                } else {
                    finish(r, "completed_with_reply", None);
                }
                Ok(())
            });
        }
        if start.elapsed() >= total {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// With the worker gone, a submit never matched within its window stays
/// uncertain for good: `delivery_unknown`.
pub(crate) fn unconfirmed_after_exit(paths: &Paths, id: &str) -> Result<()> {
    change(paths, id, |r| {
        let window = confirm_window(r).as_millis() as i64;
        let Some(t) = r.terminal.as_ref() else {
            return Ok(());
        };
        let late = t
            .submitted_at_ms
            .is_some_and(|at| now_ms() - at > window + 5000);
        if !is_terminal(r) && r.facts.written && t.input_uuid.is_none() && late {
            finish(r, "delivery_unknown", Some("TERMINAL_INPUT_UNCONFIRMED"));
        }
        Ok(())
    })
}

// ---------------------------------------------------------------- receipts

fn label_for(mode: &str) -> &'static str {
    match mode {
        "queue" => LABEL_QUEUE,
        "steer" => LABEL_STEER,
        _ => LABEL,
    }
}
/// Receipt fields of an Antigravity terminal record (after the shared ones).
pub fn receipt_fields(r: &Record, v: &mut Value) {
    let Some(t) = &r.terminal else {
        return;
    };
    v["terminal_input"]["label"] = json!(LABEL);
    let a = t.antigravity.clone().unwrap_or_default();
    v["antigravity_input"] = json!({"label":label_for(&r.request.mode),"queue_mode":a.queue_mode,
        "queue_mode_source":a.queue_mode_source,"busy_at_submit":a.busy_at_submit,"key":a.key,
        "expected":a.expected,"screen_evidence":a.screen_evidence,"outcome":a.outcome,
        "caveat":(r.request.mode == "steer").then_some(STEER_CAVEAT)});
    if a.screen_evidence.is_some() {
        v["facts"]["accepted_native"] = json!(true);
    }
}

// ------------------------------------------------------- session capability

pub fn steer_semantics() -> Value {
    json!({"priority":"next","interrupts":false,"delivery":"sent_to_running_turn",
        "interrupt_option":null,"label":LABEL_STEER,"caveat":STEER_CAVEAT})
}

/// Per-session submit/queue/steer for an Antigravity session row.
pub fn apply_session_input(value: &mut Value, home: Option<&Path>, terminal_enabled: bool) {
    let running = value["availability"] == "running";
    let typed = super::terminal_reason(value, terminal_enabled);
    let pid = value["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
    let (q, source) = match home {
        Some(home) => queue_mode_of(home, pid),
        None => (None, "unknown"),
    };
    let mut modes: [(&str, Option<&str>, Option<String>); 3] = [
        ("submit", None, None),
        ("queue", None, None),
        ("steer", None, None),
    ];
    let terminal_reason: Option<String> = if !running {
        Some("SESSION_NOT_RUNNING".into())
    } else {
        typed.map(str::to_owned)
    };
    for m in &mut modes {
        if !running {
            m.2 = Some(
                "SESSION_NOT_RUNNING: use mode resume (headless) for a stopped Antigravity session"
                    .into(),
            );
        } else if let Some(code) = typed {
            m.2 = Some(format!(
                "{code}: Antigravity takes live input only as typed input into a verified HERDR pane"
            ));
        } else if m.0 != "submit" && q.is_none() {
            m.2 = Some(QUEUE_MODE_UNKNOWN.into());
        } else {
            m.1 = Some(terminal::SEMANTICS);
        }
    }
    for (k, path, reason) in &modes {
        value["capabilities"][*k] = json!(path.is_some());
        value["capability_reasons"][*k] = json!(reason);
    }
    let available = terminal_reason.is_none();
    let path = modes[0].1;
    value["input_delivery"] = json!({"semantics":path,"interrupt":false,
        "label":path.map(|_| LABEL),
        "steer":modes[2].1.map(|_| steer_semantics()),
        "modes":{"submit":modes[0].1,"queue":modes[1].1,"steer":modes[2].1},
        "available":if available {vec![terminal::SEMANTICS]} else {vec![]},
        "terminal_input":{"available":available,"reason":terminal_reason},
        "queued_messages":{"value":q.map(|q| q.as_str()),"source":source}});
}

/// `capabilities.antigravity_input_delivery`.
pub fn input_delivery() -> Value {
    json!({"version":1,"since_revision":24,"semantics":terminal::SEMANTICS,"modes":["submit","queue","steer"],
        "label":LABEL,"labels":{"submit":LABEL,"queue":LABEL_QUEUE,"steer":LABEL_STEER},
        "herdr_agent":AGENT,
        "eligibility":"tracked running Antigravity session whose attachment is a verified HERDR pane (attachment.host herdr, exact pid/start)",
        "keys_by_queued_messages":{"queue":{"queue":"enter","steer":"alt+m"},"send-immediately":{"queue":"alt+m","steer":"enter"}},
        "enter":{"idle":"user_turn","busy":"per queuedMessages (default queue)"},
        "queued_messages":{"setting":"queuedMessages","values":["queue","send-immediately"],"default":"queue",
            "sources":["--settings inline JSON on the process command line","HOME/.gemini/antigravity-cli/settings.json"],
            "unknown":"queue and steer refused (ANTIGRAVITY_QUEUE_MODE_UNKNOWN); submit allowed"},
        "steer":steer_semantics(),
        "keys":{"text":"pane send-text (no bracketed paste)","newline":"ctrl+j","clear":["alt+>","ctrl+u","backspace"],
            "submit":["enter","alt+m"],"never":["esc","ctrl+c","ctrl+d","ctrl+k","ctrl+l","up","down"]},
        "refused_prefixes":["/","?","!"],"max_text_bytes":MAX_TEXT,"max_lines":MAX_LINES,
        "draft":"read from the screen, cleared, typed back unsubmitted and verified; refused when not exactly reconstructable",
        "views":"only the main prompt with its one-row footer; menus, panels, pickers, dialogs and an armed exit are refused",
        "correlation":"first explicit USER_INPUT step in brain/<id>/.system_generated/logs/transcript.jsonl at or after the submit whose <USER_REQUEST> contains the typed text","reply_requires_anchor":true,
        "refusal_reasons":REASONS,"automatic_replay":false,"interrupt":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::claude_steer::terminal::cells;

    fn screen(name: &str) -> Vec<Vec<Cell>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/antigravity-screens")
            .join(name);
        cells(&fs::read_to_string(&path).unwrap_or_else(|_| panic!("{}", path.display())))
    }

    #[test]
    fn real_screens_classify_idle_busy_drafts_and_refuse_overlays() {
        let main = |name: &str| classify(&screen(name), true);
        assert_eq!(
            main("idle.ansi"),
            Ok(View::Main {
                draft: String::new(),
                busy: false
            })
        );
        assert_eq!(
            main("busy.ansi"),
            Ok(View::Main {
                draft: String::new(),
                busy: true
            })
        );
        assert_eq!(
            main("busy-draft.ansi"),
            Ok(View::Main {
                draft: "queued: reply QUEUED-OK".into(),
                busy: true
            })
        );
        assert_eq!(
            main("idle-draft-multiline.ansi"),
            Ok(View::Main {
                draft: "draft line one\ndraft line two".into(),
                busy: false
            })
        );
        assert_eq!(
            main("busy-queued.ansi"),
            Ok(View::Main {
                draft: String::new(),
                busy: true
            })
        );
        assert_eq!(
            main("slash-menu.ansi"),
            Err("TERMINAL_OVERLAY_UNIDENTIFIED")
        );
        assert_eq!(main("usage.ansi"), Err("TERMINAL_OVERLAY_UNIDENTIFIED"));
        assert_eq!(
            main("model-menu.ansi"),
            Err("TERMINAL_OVERLAY_UNIDENTIFIED")
        );
        assert_eq!(main("trust.ansi"), Err("TERMINAL_VIEW_UNDETERMINED"));
        // A tool permission dialog (HERDR also reports the pane `blocked`).
        assert_eq!(
            main("permission.ansi"),
            Err("TERMINAL_OVERLAY_UNIDENTIFIED")
        );
    }

    #[test]
    fn exit_armed_footer_is_refused() {
        let mut rows = screen("idle.ansi");
        let last = rows
            .iter()
            .rposition(|r| row_text(r).contains("? for shortcuts"))
            .unwrap();
        let width = rows[last].len();
        let text = format!("{:<width$}", "press ctrl+c again to exit");
        rows[last] = text
            .chars()
            .map(|ch| Cell {
                ch,
                style: Default::default(),
            })
            .collect();
        assert_eq!(classify(&rows, true), Err("TERMINAL_EXIT_ARMED"));
    }

    #[test]
    fn screen_evidence_reads_the_queue_and_sending_rows() {
        let lines: Vec<String> = screen("steer-and-queue.ansi")
            .iter()
            .map(|r| row_text(r))
            .collect();
        assert_eq!(
            screen_evidence(&lines, "steer note: also say STEERED"),
            Some("sending")
        );
        assert_eq!(
            screen_evidence(&lines, "queued note: reply QUEUED"),
            Some("queued")
        );
        assert_eq!(screen_evidence(&lines, "something else entirely"), None);
    }

    #[test]
    fn payload_rules() {
        assert_eq!(deliverable(" hi there \r\n").unwrap().0, "hi there");
        for bad in ["/help", "?", "!ls", "end with \\", "see @file", "a\tb"] {
            assert!(deliverable(bad).is_err(), "{bad}");
        }
        assert!(deliverable(&"x".repeat(MAX_TEXT + 1)).is_err());
    }

    #[test]
    fn keys_follow_the_queued_messages_setting() {
        use QueueMode::*;
        assert_eq!(
            busy_key("queue", Some(Queue)),
            Ok(("enter", "native_queue"))
        );
        assert_eq!(
            busy_key("steer", Some(Queue)),
            Ok(("alt+m", "send_immediately"))
        );
        assert_eq!(
            busy_key("queue", Some(SendImmediately)),
            Ok(("alt+m", "native_queue"))
        );
        assert_eq!(
            busy_key("steer", Some(SendImmediately)),
            Ok(("enter", "send_immediately"))
        );
        assert_eq!(
            busy_key("submit", Some(Queue)),
            Ok(("enter", "native_queue"))
        );
        assert_eq!(busy_key("submit", None), Ok(("enter", "unknown")));
        assert!(busy_key("steer", None).is_err() && busy_key("queue", None).is_err());
    }

    #[test]
    fn transcript_anchor_and_replies_from_the_real_step_format() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("transcript.jsonl");
        let rec = |i: u32, source: &str, ty: &str, at: &str, content: &str| {
            json!({"step_index":i,"source":source,"type":ty,"status":"DONE","created_at":at,"content":content}).to_string()
        };
        let req = |t: &str| {
            format!(
                "<USER_REQUEST>\n{t}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: x.\n</ADDITIONAL_METADATA>"
            )
        };
        let lines = [
            rec(
                0,
                "USER_EXPLICIT",
                "USER_INPUT",
                "2026-01-01T00:00:00Z",
                &req("start the job"),
            ),
            rec(1, "MODEL", "PLANNER_RESPONSE", "2026-01-01T00:00:01Z", ""),
            rec(
                2,
                "MODEL",
                "GENERIC",
                "2026-01-01T00:00:02Z",
                "Tool is running",
            ),
            rec(
                3,
                "USER_EXPLICIT",
                "USER_INPUT",
                "2026-01-01T00:00:10Z",
                &req("steer note: also say STEERED"),
            ),
            rec(
                4,
                "MODEL",
                "PLANNER_RESPONSE",
                "2026-01-01T00:00:11Z",
                "Noted.",
            ),
            rec(
                5,
                "SYSTEM",
                "SYSTEM_MESSAGE",
                "2026-01-01T00:00:12Z",
                "system",
            ),
            rec(
                6,
                "MODEL",
                "PLANNER_RESPONSE",
                "2026-01-01T00:00:13Z",
                "FINISHED\n\nSTEERED",
            ),
            rec(
                7,
                "USER_EXPLICIT",
                "USER_INPUT",
                "2026-01-01T00:00:14Z",
                &req("queued note: reply QUEUED"),
            ),
            rec(
                8,
                "MODEL",
                "PLANNER_RESPONSE",
                "2026-01-01T00:00:14Z",
                "QUEUED",
            ),
        ];
        fs::write(&p, lines.join("\n") + "\n").unwrap();
        let since = timestamp_ms("2026-01-01T00:00:05Z").unwrap();
        let steer = find(&p, 0, "steer note:  also say STEERED", since).unwrap();
        assert_eq!(steer.id.as_deref(), Some("3"));
        assert!(steer.exact && steer.turn_ended);
        assert_eq!(steer.reply, ["Noted.", "FINISHED\n\nSTEERED"]);
        let queued = find(&p, 0, "queued note: reply QUEUED", since).unwrap();
        assert_eq!(
            (queued.id.as_deref(), queued.turn_ended),
            (Some("7"), false)
        );
        assert_eq!(queued.reply, ["QUEUED"]);
        let late = timestamp_ms("2026-01-01T00:00:20Z").unwrap();
        assert_eq!(
            find(&p, 0, "queued note: reply QUEUED", late).unwrap().id,
            None
        );
        assert_eq!(
            find(&p, 0, "start the job", since).unwrap().id,
            None,
            "before the submit"
        );
    }
}
