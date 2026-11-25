//! Antigravity CLI 1.4.10 terminal input (contract revision 24): submit, queue
//! and steer for an interactive agy session that Sessionsation tracks and that
//! runs in a verified HERDR pane (HERDR agent kind `agy`). It is a [`ScreenProfile`]
//! of the typing engine ([`super::typing`]): HERDR typed input without
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
    Record, Request, abandon_unsent, change, finish, is_terminal, pane_lock, read,
    terminal::{
        self, CONFIRM_SECONDS, Cell, Host, Terminal, columns, now_ms, reason, row_text, squash,
        timestamp_ms,
    },
    typing::{
        self, ClearKeys, Draft, Key, Prefix, Screen, ScreenProfile, SessionInput, Typing, code,
    },
    worker_lock,
};
use crate::{
    catalog,
    paths::{Paths, lock_file},
    provider::{ProviderId, antigravity, antigravity::QueueMode, claude},
};
use anyhow::{Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Read, Seek},
    path::Path,
    time::Duration,
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
pub const MAX_TEXT: usize = 8192;
pub const MAX_LINES: usize = 200;
const DRAFT_ROWS: usize = 9;
/// Rows of the (tail-only) input box read back from a typed message.
const TYPED_ROWS: usize = 120;
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
    "TERMINAL_PANE_IS_SHELL",
    "TERMINAL_PROVIDER_EXITED",
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
    typing::deliverable::<Antigravity>(text)
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
    let box_rows = if strict { DRAFT_ROWS } else { TYPED_ROWS };
    let Some(top) = (bottom.saturating_sub(box_rows + 1)..bottom)
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
    // A long draft shows only its tail under a first row `> ↑ N more lines`.
    let scrolled = lines[p]
        .strip_prefix("> ")
        .is_some_and(|r| r.starts_with('↑') && r.trim_end().ends_with(" more lines"));
    if scrolled && strict {
        return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
    }
    let first = if scrolled { p + 1 } else { p };
    let mut content: Vec<&[Cell]> = vec![];
    for (i, row) in rows.iter().enumerate().take(bottom).skip(first) {
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

/// Antigravity CLI 1.4.10's terminal for the typing engine, with the session's
/// queuedMessages read at the recheck: the box between full-width rules with a `>`
/// prompt and a one-row footer ([`classify`]), alt+> then ctrl+u / backspace clearing,
/// typed in verified segments, Enter or alt+m per the mode and the setting.
pub(crate) struct Antigravity {
    queue_mode: Option<QueueMode>,
}
impl ScreenProfile for Antigravity {
    const AGENT: &'static str = AGENT;
    const PROVIDER: ProviderId = ProviderId::Antigravity;
    const PROCESS: &'static str = "agy";
    const MAX_TEXT: usize = MAX_TEXT;
    const MAX_LINES: Option<usize> = Some(MAX_LINES);
    const PREFIX: Prefix = Prefix::Refuse(
        &['/', '?', '!'],
        "TERMINAL_PAYLOAD_UNSUPPORTED: agy treats a leading / ? or ! as a command, its shortcuts or a mode; rephrase the start of the text",
    );
    /// alt+> (end of input), then per round ctrl+u (kill to the line start) and backspace
    /// (join the previous line). None of these keys does anything else in agy's prompt.
    const CLEAR: ClearKeys = ClearKeys {
        first: &["alt+>"],
        round: &["ctrl+u", "backspace"],
        row_chars: 30,
        rounds_per_row: 1,
        extra: 2,
        max: 400,
    };
    const VIEW_SWITCHES: usize = 0;
    const TYPING: Typing = Typing::Checked { reread: true };
    const DRAFT: Draft = Draft::Kept;
    const ONE_BLOCKED_READ: bool = false;
    const RESTORE_ERROR_NAMES_HOST: bool = false;
    const REREAD_DRAFT: bool = false;
    const REFUSAL_LAYOUT: bool = false;
    const MODE_GATED: bool = true;
    fn too_long() -> String {
        format!(
            "TERMINAL_TEXT_TOO_LONG: terminal input into agy takes at most {MAX_TEXT} bytes and {MAX_LINES} lines"
        )
    }
    fn classify(rows: &[Vec<Cell>], strict: bool) -> Screen {
        classify(rows, strict).map(|View::Main { draft, busy }| typing::View::Main { draft, busy })
    }
    /// Typing it back would open a menu, the shortcuts or a completion.
    fn restorable(draft: &str) -> bool {
        let last = draft.split_whitespace().last().unwrap_or_default();
        !(draft.starts_with(['?', '!', '/'])
            || last.starts_with(['@', '/'])
            || draft.contains('\t'))
    }
    fn observe_process(
        home: &Path,
        conversation: &str,
        attachment: Option<&catalog::Attachment>,
    ) -> claude::Observation {
        antigravity::observe(home, conversation, attachment)
    }
    fn refusal(&self, mode: &str, busy: bool) -> Option<&'static str> {
        if busy {
            busy_key(mode, self.queue_mode).err()
        } else {
            None
        }
    }
    /// Idle Enter is a user turn; busy follows the setting.
    fn key(&self, mode: &str, busy: bool) -> Result<Key, &'static str> {
        let (key, expected) = if busy {
            busy_key(mode, self.queue_mode)?
        } else {
            ("enter", "user_turn")
        };
        Ok(Key {
            key,
            expected: Some(expected),
        })
    }
    fn anchor(&self, paths: &Paths, id: &str, _t: &Terminal) -> Result<Option<u64>> {
        Ok(antigravity_log(paths, &read(paths, id)?)
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len()))
    }
    fn submit_facts(&self, t: &mut Terminal, busy: bool, key: &Key) {
        let f = t.antigravity.get_or_insert_with(Facts::default);
        f.busy_at_submit = Some(busy);
        f.key = Some(key.key.into());
        f.expected = key.expected.map(Into::into);
    }
    /// A busy key shows agy's queue row or "Sending".
    fn evidence(&self, lines: &[String], text: &str) -> Option<&'static str> {
        screen_evidence(lines, text)
    }
    fn record_evidence(t: &mut Terminal, evidence: &str) {
        t.antigravity
            .get_or_insert_with(Facts::default)
            .screen_evidence = Some(evidence.into());
    }
    fn confirm_window(r: &Record) -> Duration {
        confirm_window(r)
    }
    fn apply_transcript(paths: &Paths, id: &str, idle: bool) -> Result<()> {
        observe(paths, id, idle)
    }
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
    let (t, home) = match typing::recheck::<Antigravity>(paths, &r) {
        Ok((t, home, _)) => (t, home),
        Err(e) => return abandon_unsent(paths, id, &reason(&e)),
    };
    let (q, source) = queue_mode_of(&home, Some(t.pid));
    if matches!(r.request.mode.as_str(), "queue" | "steer") && q.is_none() {
        return abandon_unsent(paths, id, code(QUEUE_MODE_UNKNOWN));
    }
    let pane_lock = pane_lock(paths, &t.target)?;
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
    let outcome = typing::deliver(&Antigravity { queue_mode: q }, paths, id, &host, &t).await;
    if let Err(e) = outcome {
        typing::worker_failed(paths, id, &e)?;
    }
    drop(pane_lock);
    typing::observe_loop::<Antigravity>(paths, id, &host).await
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

/// Per-session submit/queue/steer for an Antigravity session row, through
/// [`typing::apply_session_input`].
impl SessionInput for Antigravity {
    const NAME: &'static str = "Antigravity";
    const LABEL: &'static str = LABEL;
    type Gates = (Option<QueueMode>, &'static str);
    fn gates(value: &Value, home: Option<&Path>) -> Self::Gates {
        let pid = value["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
        match home {
            Some(home) => queue_mode_of(home, pid),
            None => (None, "unknown"),
        }
    }
    fn mode_reason(&(q, _): &Self::Gates, mode: &str) -> Option<String> {
        (mode != "submit" && q.is_none()).then(|| QUEUE_MODE_UNKNOWN.into())
    }
    fn steer_semantics() -> Value {
        steer_semantics()
    }
    fn row_extra((q, source): Self::Gates) -> (&'static str, Value) {
        (
            "queued_messages",
            json!({"value":q.map(|q| q.as_str()),"source":source}),
        )
    }
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
mod tests;
