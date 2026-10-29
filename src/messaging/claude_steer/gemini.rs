//! Gemini CLI 0.66.0 terminal input (contract revision 17): submit, queue and
//! steer for an interactive Gemini session that Sessionsation tracks and that
//! runs in a verified HERDR pane. It reuses the Claude terminal-input machinery
//! ([`super::terminal`]): HERDR typed input without bracketed paste, the draft
//! round-trip, screen checks, journaling before every write, transcript
//! anchoring and no replay.
//!
//! Gemini CLI has no live input API that fits: there is no stream-json input,
//! and its ACP mode cancels a running turn when a new prompt arrives. What
//! Enter does in the TUI depends on the session's state and on the account's
//! `experimental.modelSteering` setting (read once at startup, see
//! [`crate::provider::gemini::steering`]):
//! - idle: a normal user turn (`submit`; `queue`/`steer` on an idle session
//!   also become this, reported as `outcome:"submitted"`).
//! - busy, steering off: the native message queue (`queue`; a busy `submit`
//!   too). Every queued message is joined with a blank line into one user
//!   prompt when the turn ends, so the anchoring record may carry other text.
//! - busy, steering on: a "user steering update" added to the next tool
//!   response (`steer`). If the turn ends without another tool call, Gemini
//!   submits the update on its own as a new prompt once idle; the receipt says
//!   which happened (`gemini_input.steering`).
//!
//! Keys: text with `pane send-text`, newlines with ctrl+j, clearing with
//! ctrl+e, ctrl+u, ctrl+k and backspace, the shortcut panel closed with
//! backspace, one Enter at least 30 ms after the last typed key (Gemini turns a
//! faster Enter into a newline). Esc and Ctrl+C cancel a running request in
//! Gemini and are never sent; neither are Ctrl+D or Ctrl+L.
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
    provider::{ProviderId, claude, gemini, gemini::steering},
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

pub const AGENT: &str = "gemini";
pub const LABEL: &str = "Typed into the session's HERDR terminal as the user's own input: Gemini records a user turn when idle; mid-turn Enter queues it (model steering off) or adds a user steering update (model steering on)";
pub const LABEL_QUEUE: &str = "Queued in Gemini's own message queue as the user's input: run when the current turn ends, joined with any other queued messages into one prompt";
pub const LABEL_STEER: &str = "Steer";
pub const STEER_CAVEAT: &str = "Gemini adds a steer to the next tool step of the running turn. If the turn ends without another tool call, Gemini submits it afterwards as a separate prompt wrapped as a steering update; the receipt reports which happened";
/// Gemini's input box shows 10 rows; typed text must stay verifiable on screen.
pub const MAX_TEXT: usize = 1000;
pub const MAX_LINES: usize = 8;
const DRAFT_ROWS: usize = 9;
/// Text Gemini puts before a steering update (`buildUserSteeringHintPrompt`).
const STEERING_PREFIX: &str = "User steering update:";
const MAX_SCAN: u64 = 16 * 1024 * 1024;
const PLACEHOLDERS: &[&str] = &[
    "Type your message or @path/to/file",
    "Type your message or hold space to talk (Esc to exit)",
    "Type your message or space to talk (Esc to exit)",
];

pub const REASONS: &[&str] = &[
    "TERMINAL_NOT_ATTACHED",
    "TERMINAL_INTERRUPT_UNSUPPORTED",
    "TERMINAL_PAYLOAD_UNSUPPORTED",
    "TERMINAL_TEXT_TOO_LONG",
    "TERMINAL_HOST_UNAVAILABLE",
    "TERMINAL_AGENT_UNKNOWN",
    "TERMINAL_AGENT_BLOCKED",
    "TERMINAL_VIEW_UNDETERMINED",
    "TERMINAL_OVERLAY_UNIDENTIFIED",
    "TERMINAL_VIM_MODE",
    "TERMINAL_SHELL_MODE",
    "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
    "TERMINAL_DRAFT_UNRESTORABLE",
    "TERMINAL_CLEAR_UNVERIFIED",
    "TERMINAL_TYPED_TEXT_MISMATCH",
    "TERMINAL_HOST_FAILED_BEFORE_SUBMIT",
    "TERMINAL_WORKER_EXITED_BEFORE_SUBMIT",
    "TERMINAL_SUBMIT_UNCERTAIN",
    "TERMINAL_INPUT_UNCONFIRMED",
    "GEMINI_STEERING_DISABLED",
    "GEMINI_STEERING_RESTART_REQUIRED",
    "GEMINI_STEERING_UNKNOWN",
    "GEMINI_QUEUE_WOULD_STEER",
    "GEMINI_SUBMIT_WOULD_STEER",
];

/// Gemini-specific delivery facts kept in the journal and the receipt.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    /// The session's `experimental.modelSteering` (None: unknown).
    pub model_steering: Option<bool>,
    pub steering_source: Option<String>,
    /// HERDR or the screen showed a running turn just before Enter.
    pub busy_at_submit: Option<bool>,
    /// `queued` | `steering_hint`: what the screen showed right after a busy Enter.
    pub screen_evidence: Option<String>,
    /// `submitted` | `queued` | `steering_applied` | `steering_after_turn`
    pub outcome: Option<String>,
    /// The anchoring user record carries more than this text (a queued batch,
    /// or a prompt Gemini expanded).
    pub batched: Option<bool>,
}

// ---------------------------------------------------------------- payload

/// The text as typed: CRLF normalized, trimmed; a leading `/`, `!` or `?`
/// gets a backslash so it stays plain text (command, shell mode, shortcuts).
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
        "TERMINAL_TEXT_TOO_LONG: Gemini's input box shows 10 rows, so terminal input takes at most {MAX_TEXT} bytes and {MAX_LINES} lines"
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
    let escaped = trimmed.starts_with(['/', '!', '?']);
    let out = if escaped {
        format!("\\{trimmed}")
    } else {
        trimmed.to_owned()
    };
    Ok((
        out,
        escaped,
        normalized.len() != text.len() || trimmed != normalized,
    ))
}

// ---------------------------------------------------------------- screen

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum View {
    /// The input prompt with no overlay; the draft (empty when none) and
    /// whether a running turn is shown above it.
    Main { draft: String, busy: bool },
    /// The shortcut panel over an empty prompt (backspace closes it).
    Help,
}

/// A border row of the input box: half blocks (background colour) or a line.
fn is_border(row: &[Cell]) -> bool {
    let t = row_text(row);
    let t = t.trim();
    let n = t.chars().count();
    n >= 8
        && t.chars()
            .all(|c| matches!(c, '▀' | '▄' | '─' | '━' | '╭' | '╮' | '╰' | '╯'))
        && n * 2 >= row.len()
}
/// The prompt marker of a row: `>` normal, `*` YOLO, `!` shell mode, `(r:)`
/// reverse search; with the column of its first character.
fn marker(row: &[Cell]) -> Option<(&'static str, usize)> {
    let t = row_text(row);
    let lead = t.len() - t.trim_start_matches(' ').len();
    if lead > 2 {
        return None;
    }
    let rest = &t[lead..];
    let m = if rest.starts_with("(r:)") {
        "(r:)"
    } else if rest == ">" || rest.starts_with("> ") {
        ">"
    } else if rest == "*" || rest.starts_with("* ") {
        "*"
    } else if rest == "!" || rest.starts_with("! ") {
        "!"
    } else {
        return None;
    };
    Some((m, lead))
}
fn visible(c: &Cell) -> bool {
    c.ch != ' ' && c.ch != '\u{a0}'
}

/// Classify a Gemini screen. `strict` reads a user draft (any doubt refuses);
/// not strict reads back text the worker typed (compared without whitespace).
pub(crate) fn classify(rows: &[Vec<Cell>], strict: bool) -> Result<View, &'static str> {
    let lines: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    let mismatch = if strict {
        "TERMINAL_DRAFT_UNRECONSTRUCTABLE"
    } else {
        "TERMINAL_TYPED_TEXT_MISMATCH"
    };
    let Some(p) = rows.iter().rposition(|r| marker(r).is_some()) else {
        return Err("TERMINAL_VIEW_UNDETERMINED");
    };
    let (kind, col) = marker(&rows[p]).expect("marker");
    match kind {
        "!" => return Err("TERMINAL_SHELL_MODE"),
        "(r:)" => return Err("TERMINAL_OVERLAY_UNIDENTIFIED"),
        _ => {}
    }
    if p == 0 || !is_border(&rows[p - 1]) {
        return Err("TERMINAL_VIEW_UNDETERMINED");
    }
    // Gemini's input viewport is 10 rows: a full box may hide scrolled rows.
    let Some(end) = (p + 1..rows.len().min(p + 1 + DRAFT_ROWS + 1)).find(|&i| is_border(&rows[i]))
    else {
        return Err(mismatch);
    };
    if end - p > DRAFT_ROWS {
        return Err(mismatch);
    }
    let footer = &lines[end + 1..];
    let above = &lines[p.saturating_sub(24)..p];
    let all = || {
        footer
            .iter()
            .chain(above.iter())
            .chain(lines[p..end].iter())
    };
    if all().any(|l| {
        l.contains("Press 'Esc' for NORMAL mode.")
            || l.contains("Press 'i' for INSERT mode.")
            || l.contains("[NORMAL]")
            || l.contains("[INSERT]")
            || l.contains("-- NORMAL --")
            || l.contains("-- INSERT --")
    }) {
        return Err("TERMINAL_VIM_MODE");
    }
    if lines[p..end]
        .iter()
        .any(|l| l.contains("Type your shell command"))
    {
        return Err("TERMINAL_SHELL_MODE");
    }
    // Below the box only the footer (or completions, which are refused).
    let suspicious = footer.iter().filter(|l| !l.trim().is_empty()).count() > 3
        || footer.iter().any(|l| {
            let t = l.trim_start();
            t.starts_with('>')
                || ["│", "╭", "╰", "┌", "└", "(r:)", "Loading suggestions"]
                    .iter()
                    .any(|m| l.contains(m))
                || l.to_ascii_lowercase().contains("esc to")
                || l.to_ascii_lowercase().contains("enter to")
        });
    if suspicious {
        return Err("TERMINAL_OVERLAY_UNIDENTIFIED");
    }
    let help = above.iter().any(|l| l.contains("See /help for more"));
    let busy = above.iter().any(|l| l.contains("esc to cancel"));
    let indent = col + 2;
    let width = rows[p - 1].len();
    let mut content: Vec<&[Cell]> = vec![];
    for (i, row) in rows.iter().enumerate().take(end).skip(p) {
        if i > p && row.iter().take(indent).any(visible) {
            return Err(mismatch);
        }
        content.push(row.get(indent..).unwrap_or(&[]));
    }
    let as_text = |r: &[Cell]| -> String {
        r.iter()
            .map(|c| if c.ch == '\u{a0}' { ' ' } else { c.ch })
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let first = as_text(content[0]);
    let draft = if content.len() == 1 && PLACEHOLDERS.contains(&first.trim()) {
        String::new()
    } else if content.iter().all(|r| !r.iter().any(visible)) {
        if content.len() > 1 {
            return Err(mismatch);
        }
        String::new()
    } else {
        if strict {
            // One text colour only: a second one is ghost text, a completion
            // or a highlighted token that cannot be told apart reliably.
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
            // Gemini wraps at word boundaries: a row that could not have taken
            // the next row's first word may be a soft wrap, not a newline.
            let room = width.saturating_sub(indent + 3);
            for (i, row) in content.iter().enumerate() {
                let used = columns(
                    as_text(row)
                        .as_str()
                        .chars()
                        .map(cell)
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
                    if used
                        + 1
                        + lead
                        + word
                            .chars()
                            .map(|c| if c.is_ascii() { 1 } else { 2 })
                            .sum::<usize>()
                        >= room
                    {
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
        if strict && (draft.contains("[Pasted Text") || draft.contains("[Image")) {
            return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
        }
        draft
    };
    if help {
        return if draft.is_empty() {
            Ok(View::Help)
        } else {
            Err("TERMINAL_OVERLAY_UNIDENTIFIED")
        };
    }
    Ok(View::Main { draft, busy })
}
fn cell(ch: char) -> Cell {
    Cell {
        ch,
        style: Default::default(),
    }
}
fn is_main(v: &Result<View, &'static str>, f: impl Fn(&str) -> bool) -> bool {
    matches!(v, Ok(View::Main { draft, .. }) if f(draft))
}

/// Read the screen until `want` holds or the wait ends; returns the last view.
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
/// Clear the input: per round ctrl+e (end of line), ctrl+u (kill to line
/// start), ctrl+k (join the next line) and backspace (join the previous one).
/// None of these keys does anything else in Gemini's prompt.
async fn clear(host: &Host, text: &str) -> Result<()> {
    let rows = text
        .split('\n')
        .map(|l| l.chars().count() / 30 + 1)
        .sum::<usize>();
    for _ in 0..(rows + 2).min(100) {
        host.keys(&["ctrl+e", "ctrl+u", "ctrl+k", "backspace"])?;
    }
    Ok(())
}

// ---------------------------------------------------------------- admission

/// What the session's own settings make of Enter mid-turn.
fn steering_of(home: &Path, o: &claude::Observation) -> steering::Effective {
    let env = o.pid.and_then(claude::environ);
    steering::effective(
        home,
        o.directory.as_deref().map(Path::new),
        env.as_deref(),
        o.process_start.and_then(claude::process_started_at),
    )
}
/// The refusal a mode gets from the session's steering state, if any.
/// `busy` is None at admission (unknown); submit then always passes.
fn mode_refusal(mode: &str, s: &steering::Effective, busy: Option<bool>) -> Option<&'static str> {
    let unknown = s.reason.unwrap_or(steering::UNKNOWN);
    match (mode, s.enabled) {
        ("steer", Some(true)) | ("queue", Some(false)) => None,
        ("steer", Some(false)) => Some(steering::DISABLED),
        ("queue", Some(true)) => Some(steering::QUEUE_WOULD_STEER),
        ("steer" | "queue", None) => Some(unknown),
        (_, Some(false)) => None,
        (_, Some(true)) if busy == Some(true) => Some(
            "GEMINI_SUBMIT_WOULD_STEER: the session is running a turn and has model steering on, so Enter would add a steering update; use steer",
        ),
        (_, None) if busy == Some(true) => Some(unknown),
        _ => None,
    }
}
fn code(s: &str) -> &str {
    s.split(':').next().unwrap_or(s)
}

/// Admission of a Gemini submit/queue/steer: the terminal target and facts.
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
        "DELIVERY_UNAVAILABLE: Gemini sessions take terminal_input only"
    );
    ensure!(
        !request.steering.as_ref().is_some_and(|s| s.interrupt),
        "TERMINAL_INTERRUPT_UNSUPPORTED: terminal input never sends Esc or Ctrl+C; omit steering.interrupt"
    );
    let o = gemini::observe(home, &session.conversation_id, session.attachment.as_ref());
    ensure!(
        o.availability == "running",
        "SESSION_NOT_RUNNING: submit/queue/steer need a live Gemini session in a HERDR pane; use mode resume (headless) for a stopped session"
    );
    let t = terminal::eligible(&o).map_err(|code| {
        anyhow::anyhow!(
            "{code}: terminal input needs a running session attached to a verified HERDR pane"
        )
    })?;
    let s = steering_of(home, &o);
    if let Some(refusal) = mode_refusal(&request.mode, &s, None) {
        anyhow::bail!("{refusal}");
    }
    let (text, escaped, normalized) = deliverable(&request.text)?;
    Ok(Terminal {
        transcript: o.transcript.clone(),
        delivered_text: text,
        escaped_prefix: escaped,
        normalized,
        gemini: Some(Facts {
            model_steering: s.enabled,
            steering_source: Some(s.source.into()),
            ..Default::default()
        }),
        ..t
    })
}

// ---------------------------------------------------------------- worker

/// Same catalog entry, live process and pane; the steering state re-read.
fn recheck(paths: &Paths, r: &Record) -> Result<(Terminal, steering::Effective)> {
    let account =
        crate::messaging::headless::usable_account(paths, ProviderId::Gemini, &r.account)?;
    let session = catalog::find(paths, &r.session_id)?;
    ensure!(session.conversation_id == r.conversation_id, "STALE_TARGET");
    let recorded = r.terminal.clone().context("STALE_TARGET")?;
    let o = gemini::observe(
        &account.home,
        &r.conversation_id,
        session.attachment.as_ref(),
    );
    let now = terminal::eligible(&o).map_err(|code| anyhow::anyhow!("{code}"))?;
    ensure!(
        now.pid == recorded.pid
            && now.process_start == recorded.process_start
            && now.target == recorded.target,
        "STALE_TARGET: the attached Gemini process or pane changed"
    );
    let s = steering_of(&account.home, &o);
    Ok((
        Terminal {
            transcript: now.transcript.or(recorded.transcript.clone()),
            ..recorded
        },
        s,
    ))
}

fn set_facts(paths: &Paths, id: &str, f: impl FnOnce(&mut Facts)) -> Result<()> {
    change(paths, id, |r| {
        if let Some(t) = r.terminal.as_mut() {
            f(t.gemini.get_or_insert_with(Facts::default));
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
    if r.phase != "admitted" || r.kind != "terminal" || !is_gemini(&r) {
        return Ok(());
    }
    if r.request.expires_at <= crate::daemon::now() {
        return change(paths, id, |r| {
            finish(r, "expired", Some("EXPIRED_BEFORE_DELIVERY"));
            Ok(())
        });
    }
    let (t, s) = match recheck(paths, &r) {
        Ok(x) => x,
        Err(e) => return abandon_unsent(paths, id, &reason(&e)),
    };
    if let Some(refusal) = mode_refusal(&r.request.mode, &s, None) {
        return abandon_unsent(paths, id, code(refusal));
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
        let facts = t.gemini.get_or_insert_with(Facts::default);
        facts.model_steering = s.enabled;
        facts.steering_source = Some(s.source.into());
        r.terminal = Some(t);
        Ok(())
    })?;
    let host = Host::for_agent(&t, AGENT)?;
    let outcome = deliver(paths, id, &host, &t, &s).await;
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

/// Withdraw typed text and put the draft back; then refuse with `code`.
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
    s: &steering::Effective,
) -> Result<()> {
    let mode = read(paths, id)?.request.mode;
    // 1. HERDR agent state (read-only).
    match host.agent_status() {
        Ok(Some(s)) if s == "blocked" => return reject(paths, id, "TERMINAL_AGENT_BLOCKED"),
        Ok(Some(_)) => {}
        Ok(None) => return reject(paths, id, "TERMINAL_AGENT_UNKNOWN"),
        Err(_) => return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE"),
    }
    // 2. View: the input prompt; the shortcut panel is closed with backspace.
    let Ok(screen) = host.screen() else {
        return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE");
    };
    let mut view = classify(&screen, true);
    if matches!(view, Ok(View::Help)) {
        step(paths, id, "leave_help", |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.view_switched = Some("help".into());
            }
        })?;
        host.keys(&["backspace"])?;
        view = until(host, true, |v| {
            matches!(v, Ok(View::Main { .. })) || v.is_err()
        })
        .await?;
    }
    let (draft, busy_screen) = match view {
        Ok(View::Main { draft, busy }) => (draft, busy),
        Ok(View::Help) => return reject(paths, id, "TERMINAL_VIEW_UNDETERMINED"),
        Err(code) => return reject(paths, id, code),
    };
    let last = draft.split_whitespace().last().unwrap_or_default();
    if draft.starts_with(['?', '!', '/']) || last.starts_with(['@', '/']) || draft.contains('\t') {
        // Typing it back would toggle a mode, the shortcut panel or a completion.
        return reject(paths, id, "TERMINAL_DRAFT_UNRESTORABLE");
    }
    let busy = busy_screen || working(host);
    if let Some(refusal) = mode_refusal(&mode, s, Some(busy)) {
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
    // 5. What Enter will do now: the mode must still fit.
    let busy = matches!(typed, Ok(View::Main { busy: true, .. })) || working(host);
    if let Some(refusal) = mode_refusal(&mode, s, Some(busy)) {
        return withdraw(paths, id, host, &t.delivered_text, &draft, code(refusal)).await;
    }
    let anchor = t
        .transcript
        .as_ref()
        .and_then(|p| fs::metadata(p).ok())
        .map(|m| m.len());
    let at = now_ms() - 2000;
    step(paths, id, "submit", |r| {
        if let Some(t) = r.terminal.as_mut() {
            t.anchor_offset = anchor;
            t.submitted_at_ms = Some(at);
            t.gemini.get_or_insert_with(Facts::default).busy_at_submit = Some(busy);
        }
    })?;
    // The screen read above came well after the last typed key, so Gemini's
    // 30 ms fast-return rule (Enter right after typing is a newline) is past.
    let sent = host.keys(&["enter"]);
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
    // 6. The input must empty; a busy Enter shows Gemini's queue or the hint.
    let empty = until(host, true, |v| is_main(v, str::is_empty)).await?;
    if busy {
        let lines: Vec<String> = host
            .screen()
            .map(|s| s.iter().map(|r| row_text(r)).collect())
            .unwrap_or_default();
        let head: String = squash(&t.delivered_text).chars().take(24).collect();
        let shown = |prefix: &str| {
            lines
                .iter()
                .any(|l| l.contains(prefix) && squash(l).contains(&head))
        };
        let evidence =
            if lines.iter().any(|l| l.contains("Steering Hint:")) && shown("Steering Hint:") {
                Some("steering_hint")
            } else if lines.iter().any(|l| l.contains("Queued (press")) {
                Some("queued")
            } else {
                None
            };
        if let Some(e) = evidence {
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

/// Type the draft back (no Enter) and verify it exactly; never retried.
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

pub(super) fn is_gemini(r: &Record) -> bool {
    r.provider.as_deref() == Some("gemini")
}
/// Text of a record's `content`/`displayContent`: a string, or the `text` of
/// its parts (thoughts excluded).
fn parts_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|p| p["thought"] != true)
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}
fn has_function_response(v: &Value) -> bool {
    v.as_array()
        .is_some_and(|a| a.iter().any(|p| p.get("functionResponse").is_some()))
}
fn spaced(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Found {
    pub id: Option<String>,
    /// `submitted` (a user prompt carrying the text) | `steering_applied`
    /// (a steering update with the next tool response) | `steering_after_turn`
    /// (a steering update submitted on its own after the turn)
    pub outcome: Option<&'static str>,
    pub exact: bool,
    pub reply: Vec<String>,
    pub turn_ended: bool,
}

/// Find the first user record at or after `since_ms` (and after `offset`)
/// whose text contains the typed text, then collect Gemini's text until the
/// next real user prompt. Only that record anchors a reply.
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
    let mut replies: Vec<(Option<String>, String)> = vec![];
    let typed = text.trim();
    for line in reader.split(b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(&line?) else {
            continue;
        };
        if v.get("$set").is_some() || v.get("sessionId").is_some() {
            continue;
        }
        let content = parts_text(&v["content"]);
        if out.id.is_none() {
            let fresh = v["timestamp"]
                .as_str()
                .and_then(timestamp_ms)
                .is_some_and(|t| t >= since_ms);
            if v["type"] != "user" || !fresh {
                continue;
            }
            let display = parts_text(&v["displayContent"]);
            let tool = has_function_response(&v["content"]);
            let outcome =
                if content.contains(STEERING_PREFIX) && spaced(&content).contains(&spaced(typed)) {
                    if tool {
                        "steering_applied"
                    } else {
                        "steering_after_turn"
                    }
                } else if !tool && (content.contains(typed) || display.contains(typed)) {
                    out.exact = content.trim() == typed || display.trim() == typed;
                    "submitted"
                } else {
                    continue;
                };
            if let Some(id) = v["id"].as_str() {
                out.id = Some(id.to_owned());
                out.outcome = Some(outcome);
            }
            continue;
        }
        match v["type"].as_str() {
            Some("gemini") => {
                let id = v["id"].as_str().map(str::to_owned);
                if let Some(slot) = replies.iter_mut().find(|(i, _)| i.is_some() && *i == id) {
                    // A record rewritten later (tool calls added) replaces its text.
                    slot.1 = content;
                } else {
                    replies.push((id, content));
                }
            }
            // Tool responses (with or without a steering update) continue the turn.
            Some("user") if !has_function_response(&v["content"]) => {
                out.turn_ended = true;
                break;
            }
            _ => {}
        }
    }
    out.reply = replies
        .into_iter()
        .map(|(_, t)| t)
        .filter(|t| !t.trim().is_empty())
        .collect();
    Ok(out)
}

/// Apply transcript evidence to a written Gemini terminal record (never regressed).
pub(crate) fn observe(paths: &Paths, id: &str, idle: bool) -> Result<()> {
    let r = read(paths, id)?;
    let Some(t) = r.terminal.clone() else {
        return Ok(());
    };
    if is_terminal(&r) || !r.facts.written {
        return Ok(());
    }
    let path = t.transcript.clone().or_else(|| {
        let account = gemini::account(paths, &r.account).ok()?;
        gemini::find_transcript(&account.home, &r.conversation_id)
    });
    let Some(path) = path else {
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
        let Some(uuid) = found.id else {
            r.observation_error = Some("terminal_input_not_found".into());
            return Ok(());
        };
        r.observation_error = None;
        let facts = t.gemini.get_or_insert_with(Facts::default);
        let busy = facts.busy_at_submit == Some(true);
        let outcome = match found.outcome {
            Some("submitted") if busy => "queued",
            Some(o) => o,
            None => "submitted",
        };
        facts.outcome = Some(outcome.into());
        let steering = outcome.starts_with("steering");
        facts.batched = Some(!steering && !found.exact);
        t.input_uuid = Some(uuid);
        t.input_record = Some(if steering { "steering_update" } else { "user" }.into());
        t.turn_ended |= found.turn_ended;
        let ended = t.turn_ended || (idle && !found.reply.is_empty());
        r.facts.input_recorded = true;
        r.facts.folded = outcome == "steering_applied";
        r.facts.shared_with_turn = outcome == "steering_applied" || !found.exact && !steering;
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

/// How long a submit may go unrecorded: a busy Enter that Gemini visibly
/// queued or took as a steering hint is recorded only when the turn reaches
/// it, so it may wait the whole request timeout; anything else 60 seconds.
fn confirm_window(r: &Record) -> Duration {
    let timeout = r
        .request
        .timeout_seconds
        .unwrap_or(super::DEFAULT_TIMEOUT_SECONDS);
    let pending = r
        .terminal
        .as_ref()
        .and_then(|t| t.gemini.as_ref())
        .is_some_and(|g| g.busy_at_submit == Some(true) && g.screen_evidence.is_some());
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

/// Receipt fields of a Gemini terminal record (after the shared terminal ones).
pub fn receipt_fields(r: &Record, v: &mut Value) {
    let Some(t) = &r.terminal else {
        return;
    };
    v["terminal_input"]["label"] = json!(LABEL);
    let g = t.gemini.clone().unwrap_or_default();
    let (applied, fate) = match g.outcome.as_deref() {
        Some("steering_applied") => (Some(true), Some("applied_at_next_tool_step")),
        Some("steering_after_turn") => {
            (Some(false), Some("submitted_after_turn_as_steering_prompt"))
        }
        _ if g.screen_evidence.as_deref() == Some("steering_hint") && !is_terminal(r) => {
            (None, Some("pending_next_tool_step_or_turn_end"))
        }
        _ => (None, None),
    };
    let expected = match (g.busy_at_submit, g.model_steering) {
        (Some(false), _) => Some("user_turn"),
        (Some(true), Some(true)) => Some("steering_update"),
        (Some(true), Some(false)) => Some("native_queue"),
        _ => None,
    };
    v["gemini_input"] = json!({"label":label_for(&r.request.mode),"model_steering":g.model_steering,
        "steering_source":g.steering_source,"busy_at_submit":g.busy_at_submit,"expected":expected,
        "screen_evidence":g.screen_evidence,"outcome":g.outcome,"batched":g.batched,
        "steering":{"applied":applied,"fate":fate},
        "caveat":(r.request.mode == "steer").then_some(STEER_CAVEAT)});
    if g.screen_evidence.is_some() {
        v["facts"]["accepted_native"] = json!(true);
    }
}

// ------------------------------------------------------- session capability

/// What a Gemini steer does to a running turn.
pub fn steer_semantics() -> Value {
    json!({"priority":"next","interrupts":false,"delivery":"next_tool_step_or_after_turn",
        "interrupt_option":null,"label":LABEL_STEER,"caveat":STEER_CAVEAT,
        "not_applied":"submitted_after_turn_as_steering_prompt"})
}

/// Per-session submit/queue/steer for a Gemini session row (typed input only).
pub fn apply_session_input(value: &mut Value, home: Option<&Path>) {
    let running = value["availability"] == "running";
    let typed = super::terminal_reason(value);
    let steering = home.map(|home| {
        let pid = value["pid"].as_u64().map(|p| p as u32);
        let env = pid.and_then(claude::environ);
        steering::effective(
            home,
            value["directory"].as_str().map(Path::new),
            env.as_deref(),
            value["process_start"]
                .as_u64()
                .and_then(claude::process_started_at),
        )
    });
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
                "SESSION_NOT_RUNNING: use mode resume (headless) for a stopped Gemini session"
                    .into(),
            );
        } else if let Some(code) = typed {
            m.2 = Some(format!(
                "{code}: Gemini takes live input only as typed input into a verified HERDR pane"
            ));
        } else {
            let s = steering.clone().unwrap_or(steering::Effective {
                enabled: None,
                configured: None,
                source: "default",
                restart_required: false,
                reason: Some(steering::UNKNOWN),
            });
            match mode_refusal(m.0, &s, None) {
                None => m.1 = Some(terminal::SEMANTICS),
                Some(refusal) => m.2 = Some(refusal.into()),
            }
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
        "model_steering":steering.map(|s| json!({"enabled":s.enabled,"configured":s.configured,
            "source":s.source,"restart_required":s.restart_required,
            "reason":s.reason.map(code)}))});
}

/// `capabilities.gemini_input_delivery`.
pub fn input_delivery() -> Value {
    json!({"version":1,"since_revision":17,"semantics":terminal::SEMANTICS,"modes":["submit","queue","steer"],
        "label":LABEL,"labels":{"submit":LABEL,"queue":LABEL_QUEUE,"steer":LABEL_STEER},
        "eligibility":"tracked running Gemini session whose attachment is a verified HERDR pane (attachment.host herdr, exact pid/start)",
        "enter":{"idle":"user_turn","busy_model_steering_off":"native_queue","busy_model_steering_on":"steering_update"},
        "mode_requirements":{"submit":"idle, or busy with model steering off (then queued)","queue":"model steering off","steer":"model steering on"},
        "queue":{"batching":"every queued message is joined with a blank line into one user prompt when the turn ends","record":"one user record for the batch"},
        "steer":steer_semantics(),
        "model_steering":{"setting":steering::SETTING,"default":false,"toggle":"account live-steering gemini:<name> --enable|--disable",
            "read_at":"session_start","restart_required":true},
        "keys":{"text":"pane send-text (no bracketed paste)","newline":"ctrl+j","clear":["ctrl+e","ctrl+u","ctrl+k","backspace"],
            "submit":"enter","leave_views":{"help":"backspace"},"never":["esc","ctrl+c","ctrl+d","ctrl+l"]},
        "escaped_prefixes":["/","!","?"],"max_text_bytes":MAX_TEXT,"max_lines":MAX_LINES,
        "draft":"read from the screen, cleared, typed back unsubmitted and verified; refused when not exactly reconstructable",
        "correlation":"first user record at or after the submit containing the typed text","reply_requires_anchor":true,
        "refusal_reasons":REASONS,"automatic_replay":false,"interrupt":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal::cells;
    const TOP: &str = "▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄";
    const BOTTOM: &str = "▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀";
    fn screen(above: &[&str], prompt: &[&str], footer: &[&str]) -> Vec<Vec<Cell>> {
        let mut s = String::from(" ✦ earlier output\n");
        for l in above {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str(TOP);
        s.push('\n');
        for l in prompt {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str(BOTTOM);
        for l in footer {
            s.push('\n');
            s.push_str(l);
        }
        cells(&s)
    }
    const FOOT: &[&str] = &[" ~/project        no sandbox        gemini-2.5-pro"];
    #[test]
    fn payloads_are_escaped_or_refused() {
        assert_eq!(deliverable("hi").unwrap(), ("hi".into(), false, false));
        assert_eq!(deliverable("/clear it.").unwrap().0, "\\/clear it.");
        assert_eq!(deliverable("!ls now.").unwrap().0, "\\!ls now.");
        assert_eq!(deliverable("?").unwrap().0, "\\?");
        assert_eq!(deliverable("# heading").unwrap().0, "# heading");
        for bad in ["a\tb", "ends \\", "see @a.rs", "run /x", " "] {
            assert!(deliverable(bad).is_err(), "{bad}");
        }
        assert!(deliverable(&"x".repeat(MAX_TEXT + 1)).is_err());
        assert!(deliverable(&"x\n".repeat(MAX_LINES + 1)).is_err());
    }
    #[test]
    fn the_prompt_and_draft_are_read_exactly_or_refused() {
        let empty = screen(
            &[],
            &[
                " > \u{1b}[7m \u{1b}[0m\u{1b}[38;5;245m Type your message or @path/to/file\u{1b}[39m",
            ],
            FOOT,
        );
        assert_eq!(
            classify(&empty, true),
            Ok(View::Main {
                draft: String::new(),
                busy: false
            })
        );
        let unfocused = screen(&[], &[" >   Type your message or @path/to/file"], FOOT);
        assert!(is_main(&classify(&unfocused, true), str::is_empty));
        let draft = screen(
            &[" ⠋ Thinking (esc to cancel, 3s)"],
            &[" > half a thought", "   second line\u{1b}[7m \u{1b}[0m"],
            FOOT,
        );
        assert_eq!(
            classify(&draft, true),
            Ok(View::Main {
                draft: "half a thought\nsecond line".into(),
                busy: true
            })
        );
        let help = screen(
            &[
                "────────────────────",
                " Shortcuts See /help for more",
                "  ! shell mode   @ select file",
            ],
            &[" >   Type your message or @path/to/file"],
            FOOT,
        );
        assert_eq!(classify(&help, true), Ok(View::Help));
        for (above, prompt, footer, code) in [
            (
                vec![],
                vec![" ! Type your shell command"],
                FOOT.to_vec(),
                "TERMINAL_SHELL_MODE",
            ),
            (
                vec![],
                vec![" >   Press 'i' for INSERT mode."],
                FOOT.to_vec(),
                "TERMINAL_VIM_MODE",
            ),
            (
                vec![],
                vec![" > [Pasted Text: 12 lines]"],
                FOOT.to_vec(),
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
            ),
            // Ghost text in a second colour.
            (
                vec![],
                vec![" > fix\u{1b}[38;5;245m the tests\u{1b}[39m"],
                FOOT.to_vec(),
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
            ),
            // Possible soft wrap: the next row's first word would have fit badly.
            (
                vec![],
                vec![
                    " > aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "   bbbb",
                ],
                FOOT.to_vec(),
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
            ),
            (
                vec![],
                vec![" (r:) hist"],
                FOOT.to_vec(),
                "TERMINAL_OVERLAY_UNIDENTIFIED",
            ),
            (
                vec![],
                vec![" > /mo"],
                vec![
                    "   /model   Set the model",
                    "   /mcp     Servers",
                    "   /memory  Memory",
                    "   /more    More",
                ],
                "TERMINAL_OVERLAY_UNIDENTIFIED",
            ),
        ] {
            assert_eq!(
                classify(&screen(&above, &prompt, &footer), true),
                Err(code),
                "{prompt:?}"
            );
        }
        // A dialog in place of the prompt: the last `>` row is history.
        let dialog = cells(&format!(
            "{TOP}\n > earlier question\n{BOTTOM}\n╭──────────────╮\n│ Allow once?  │\n│ ● 1. Allow   │\n╰──────────────╯"
        ));
        assert!(classify(&dialog, true).is_err());
        assert_eq!(
            classify(&cells("$ plain shell"), true),
            Err("TERMINAL_VIEW_UNDETERMINED")
        );
        let tall: Vec<String> = (0..10).map(|i| format!("   line {i}")).collect();
        let mut prompt = vec![" > first".to_owned()];
        prompt.extend(tall);
        let prompt: Vec<&str> = prompt.iter().map(String::as_str).collect();
        assert!(classify(&screen(&[], &prompt, FOOT), true).is_err());
    }
    #[test]
    fn the_anchor_is_the_first_user_record_carrying_the_text() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let lines = [
            json!({"sessionId":"x","startTime":"2026-10-29T00:00:00.000Z"}),
            json!({"id":"old","timestamp":"2026-10-29T00:00:00.000Z","type":"user","content":[{"text":"status?"}]}),
            json!({"id":"batch","timestamp":"2026-10-29T00:00:10.000Z","type":"user","content":[{"text":"first\n\nstatus?"}]}),
            json!({"id":"g1","timestamp":"2026-10-29T00:00:11.000Z","type":"gemini","content":"partial"}),
            json!({"$set":{"lastUpdated":"2026-10-29T00:00:11.000Z"}}),
            json!({"id":"g1","timestamp":"2026-10-29T00:00:11.000Z","type":"gemini","content":"working on it"}),
            json!({"id":"t","timestamp":"2026-10-29T00:00:12.000Z","type":"user","content":[{"functionResponse":{"name":"x"}}]}),
            json!({"id":"g2","timestamp":"2026-10-29T00:00:13.000Z","type":"gemini","content":"done"}),
            json!({"id":"next","timestamp":"2026-10-29T00:00:14.000Z","type":"user","content":[{"text":"another"}]}),
        ];
        fs::write(
            &p,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        let since = timestamp_ms("2026-10-29T00:00:05.000Z").unwrap();
        let f = find(&p, 0, "status?", since).unwrap();
        assert_eq!(f.id.as_deref(), Some("batch"));
        assert_eq!(f.outcome, Some("submitted"));
        assert!(!f.exact, "a queued batch shares the record");
        assert_eq!(f.reply, ["working on it", "done"]);
        assert!(f.turn_ended);
        // Steering updates: with a tool response (applied) or alone (after the turn).
        let hint = format!(
            "{STEERING_PREFIX}\n<user_input>\nalso   check\nthe docs.\n</user_input>\nInternal instruction: ..."
        );
        let lines = [
            json!({"id":"h1","timestamp":"2026-10-29T00:00:10.000Z","type":"user","content":[{"text":hint},{"functionResponse":{"name":"x"}}]}),
            json!({"id":"g","timestamp":"2026-10-29T00:00:11.000Z","type":"gemini","content":"adjusted"}),
        ];
        fs::write(
            &p,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        let f = find(&p, 0, "also check\nthe docs.", since).unwrap();
        assert_eq!(
            (f.id.as_deref(), f.outcome),
            (Some("h1"), Some("steering_applied"))
        );
        assert_eq!(f.reply, ["adjusted"]);
        let lines = [
            json!({"id":"h2","timestamp":"2026-10-29T00:00:10.000Z","type":"user","content":[{"text":hint}]}),
        ];
        fs::write(
            &p,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        let f = find(&p, 0, "also check the docs.", since).unwrap();
        assert_eq!(f.outcome, Some("steering_after_turn"));
        // Nothing after the submit: no anchor.
        assert_eq!(
            find(
                &p,
                0,
                "also check the docs.",
                timestamp_ms("2026-10-29T01:00:00.000Z").unwrap()
            )
            .unwrap(),
            Found::default()
        );
    }
    #[test]
    fn modes_follow_the_steering_setting() {
        let on = steering::Effective {
            enabled: Some(true),
            configured: Some(true),
            source: "user",
            restart_required: false,
            reason: None,
        };
        let off = steering::Effective {
            enabled: Some(false),
            ..on.clone()
        };
        let unknown = steering::Effective {
            enabled: None,
            restart_required: true,
            reason: Some(steering::RESTART_REQUIRED),
            ..on.clone()
        };
        assert_eq!(mode_refusal("steer", &on, None), None);
        assert_eq!(
            code(mode_refusal("steer", &off, None).unwrap()),
            "GEMINI_STEERING_DISABLED"
        );
        assert_eq!(
            code(mode_refusal("queue", &on, None).unwrap()),
            "GEMINI_QUEUE_WOULD_STEER"
        );
        assert_eq!(
            code(mode_refusal("queue", &unknown, None).unwrap()),
            "GEMINI_STEERING_RESTART_REQUIRED"
        );
        assert_eq!(mode_refusal("submit", &on, Some(false)), None);
        assert_eq!(
            code(mode_refusal("submit", &on, Some(true)).unwrap()),
            "GEMINI_SUBMIT_WOULD_STEER"
        );
        assert_eq!(mode_refusal("submit", &off, Some(true)), None);
        assert!(mode_refusal("submit", &unknown, Some(true)).is_some());
    }
}
