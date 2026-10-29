//! Terminal input (contract revision 17): real typed input into the HERDR pane
//! of an interactive Claude Code session that Sessionsation tracks.
//!
//! The text is typed as ordinary keystrokes (`herdr pane send-text`, which
//! writes the bytes without a bracketed-paste wrapper, and `pane send-keys`),
//! so Claude Code records it as the user's own input: a `user` record when the
//! session is idle, or a `queued_command` with `origin.kind:"human"` mid-turn.
//!
//! Procedure (one pane at a time, journaled before every write):
//! 1. HERDR must report a Claude agent on the pane that is not `blocked`.
//! 2. The screen must show the main prompt with no overlay. The detailed
//!    transcript view is left with ctrl+o (`app:toggleTranscript`, a global
//!    toggle) and the shortcut help with `?` on the empty prompt (its own
//!    toggle); a subagent view, vim mode, shell mode or any unidentified
//!    overlay is refused, because Esc would interrupt a running turn or kill
//!    the viewed subagent.
//! 3. The user's draft is read from the screen and must be reconstructable
//!    exactly (no paste/image placeholders, no possible soft wrap, no dim
//!    text mixed in); otherwise the request is refused before any key.
//! 4. The draft is cleared with ctrl+k / ctrl+u pairs (kill to end / start of
//!    the line, each also removing an adjacent newline), never Ctrl+C, Ctrl+D
//!    or Esc. Claude Code 2.1.312 binds ctrl+l (`chat:clearInput`) only to a
//!    redraw, so it is not used.
//! 5. The text is typed in small chunks with ctrl+j (`chat:newline`) between
//!    lines, checked on screen, and submitted with one Enter.
//! 6. The draft is typed back (not submitted) and checked on screen.
//! 7. The transcript is read for the human record carrying exactly the typed
//!    text after the submit; its uuid anchors the reply. Without it the
//!    receipt is `delivery_unknown`. Nothing is ever re-sent.
use super::{Record, Request, abandon_unsent, change, dir, finish, is_terminal, read, worker_lock};
use crate::{
    catalog,
    paths::{Paths, lock_file, private_dir},
    provider::{ProviderId, claude, claude::hosted},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Read, Seek},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const SEMANTICS: &str = "terminal_input";
pub const LABEL: &str = "Typed into the session's HERDR terminal as the user's own input: Claude records it as a human message (queued mid-turn, or a new turn when idle)";
/// Longest text typed into a terminal (the prompt must stay verifiable on screen).
pub const MAX_TEXT: usize = 4096;
const CHUNK: usize = 32;
const CHUNK_PAUSE: Duration = Duration::from_millis(15);
pub(crate) const SETTLE: Duration = Duration::from_millis(120);
pub(crate) const SCREEN_WAIT: Duration = Duration::from_secs(3);
/// Rows a draft may occupy and still be read back exactly.
const MAX_DRAFT_ROWS: usize = 12;
/// Rows a typed message may occupy and still be verified.
const MAX_TYPED_ROWS: usize = 60;
/// Window in which the typed input must appear in the transcript.
pub(crate) const CONFIRM_SECONDS: u64 = 60;
const MAX_SCAN: u64 = 16 * 1024 * 1024;

pub const REASONS: &[&str] = &[
    "TERMINAL_NOT_ATTACHED",
    "TERMINAL_QUEUE_UNSUPPORTED",
    "TERMINAL_INTERRUPT_UNSUPPORTED",
    "TERMINAL_PAYLOAD_UNSUPPORTED",
    "TERMINAL_TEXT_TOO_LONG",
    "TERMINAL_HOST_UNAVAILABLE",
    "TERMINAL_AGENT_UNKNOWN",
    "TERMINAL_AGENT_BLOCKED",
    "TERMINAL_VIEW_NOT_MAIN",
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
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Terminal {
    /// `herdr/<session>/<pane>`
    pub target: String,
    pub socket: Option<PathBuf>,
    pub pid: u32,
    pub process_start: u64,
    pub transcript: Option<PathBuf>,
    /// The text actually typed (prefix-escaped, trimmed).
    pub delivered_text: String,
    pub escaped_prefix: bool,
    pub normalized: bool,
    /// `transcript` | `help`: a view the worker left before typing.
    pub view_switched: Option<String>,
    /// The user's draft, kept privately until its restore is verified (never
    /// in receipts); `draft_chars` stays for the receipt.
    pub draft: Option<String>,
    pub draft_chars: Option<usize>,
    pub draft_restored: Option<bool>,
    pub restore_error: Option<String>,
    pub anchor_offset: Option<u64>,
    pub submitted_at_ms: Option<i64>,
    pub input_uuid: Option<String>,
    /// `queued_command` | `user`
    pub input_record: Option<String>,
    pub turn_ended: bool,
    pub steps: Vec<String>,
    /// Gemini CLI sessions only (see [`super::gemini`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<super::gemini::Facts>,
}

// ---------------------------------------------------------------- payload

/// The text as typed: CRLF normalized, trimmed, a leading `/`, `!`, `#` or
/// `?` escaped with a backslash so it stays plain text (command, shell,
/// memory and help triggers). Refuses text the prompt cannot take verbatim.
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
        trimmed.len() <= MAX_TEXT,
        "TERMINAL_TEXT_TOO_LONG: terminal input takes at most {MAX_TEXT} bytes"
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
    let escaped = trimmed.starts_with(['/', '!', '#', '?']);
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Style {
    pub dim: bool,
    pub fg: bool,
    pub inverse: bool,
    /// Foreground colour id (0 = default): SGR 30-37/90-97, 1000+n for
    /// 256-colour, 0x1000000|rgb for true colour.
    pub color: u32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Cell {
    pub ch: char,
    pub style: Style,
}

/// Parse `pane read --format ansi` output into styled rows (SGR only; other
/// escape sequences are skipped).
pub(crate) fn cells(ansi: &str) -> Vec<Vec<Cell>> {
    let mut rows = vec![vec![]];
    let mut style = Style::default();
    let mut it = ansi.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut fin = None;
                    for p in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&p) {
                            fin = Some(p);
                            break;
                        }
                        params.push(p);
                    }
                    if fin == Some('m') {
                        sgr(&mut style, &params);
                    }
                }
                Some(']') => {
                    // OSC: until BEL or ESC \
                    while let Some(p) = it.next() {
                        if p == '\u{7}' {
                            break;
                        }
                        if p == '\u{1b}' {
                            it.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            '\n' => rows.push(vec![]),
            c if c.is_control() => {}
            c => rows.last_mut().expect("row").push(Cell { ch: c, style }),
        }
    }
    rows
}
fn sgr(style: &mut Style, params: &str) {
    let codes: Vec<u32> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *style = Style::default(),
            2 => style.dim = true,
            22 => style.dim = false,
            7 => style.inverse = true,
            27 => style.inverse = false,
            c @ (30..=37 | 90..=97) => {
                style.fg = true;
                style.color = c;
            }
            39 => {
                style.fg = false;
                style.color = 0;
            }
            38 => {
                style.fg = true;
                let at = |k: usize| codes.get(i + k).copied().unwrap_or(0);
                if at(1) == 5 {
                    style.color = 1000 + at(2);
                    i += 2;
                } else {
                    style.color =
                        0x100_0000 | (at(2) & 0xff) << 16 | (at(3) & 0xff) << 8 | (at(4) & 0xff);
                    i += 4;
                }
            }
            48 => i += if codes.get(i + 1) == Some(&5) { 2 } else { 4 },
            _ => {}
        }
        i += 1;
    }
}
pub(crate) fn row_text(row: &[Cell]) -> String {
    row.iter().map(|c| c.ch).collect()
}
/// Columns a row occupies; anything outside ASCII counts as wide (two columns),
/// which can only make the wrap check stricter.
pub(crate) fn columns(row: &[Cell]) -> usize {
    row.iter()
        .map(|c| if c.ch.is_ascii() { 1 } else { 2 })
        .sum()
}
fn is_rule(row: &[Cell]) -> bool {
    matches!(row.first().map(|c| c.ch), Some('─' | '╭' | '╰'))
        && row.iter().filter(|c| c.ch == '─').count() * 2 > row.len()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum View {
    /// The main prompt with no overlay; the draft text (empty when none).
    Main(String),
    /// The detailed transcript view (ctrl+o toggles it).
    Transcript,
    /// The shortcut help over an empty main prompt (`?` toggles it).
    Help,
}

/// Classify a screen. `strict` reads a user draft (any doubt refuses); not
/// strict reads back text the worker typed (soft wraps allowed, dim ghost
/// text ignored, compared without whitespace by the caller).
pub(crate) fn classify(rows: &[Vec<Cell>], strict: bool) -> Result<View, &'static str> {
    let lines: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    if lines
        .iter()
        .any(|l| l.contains("Showing detailed transcript"))
    {
        return Ok(View::Transcript);
    }
    let pointer = |l: &str| {
        let t = l.trim_start_matches(' ');
        l.len() - t.len() <= 2 && (t.starts_with("❯\u{a0}") || t.starts_with("❯ ") || t == "❯")
    };
    let Some(p) = lines.iter().rposition(|l| pointer(l)) else {
        let shell = lines.iter().any(|l| l.trim_start().starts_with("!\u{a0}"));
        return Err(if shell {
            "TERMINAL_SHELL_MODE"
        } else {
            "TERMINAL_VIEW_UNDETERMINED"
        });
    };
    if p == 0 || !is_rule(&rows[p - 1]) {
        return Err("TERMINAL_VIEW_UNDETERMINED");
    }
    let limit = if strict {
        MAX_DRAFT_ROWS
    } else {
        MAX_TYPED_ROWS
    };
    let Some(end) = (p + 1..rows.len().min(p + 1 + limit)).find(|&i| is_rule(&rows[i])) else {
        return Err(if strict {
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE"
        } else {
            "TERMINAL_TYPED_TEXT_MISMATCH"
        });
    };
    let col = rows[p].iter().position(|c| c.ch == '❯').unwrap_or(0);
    if rows[p][col].style.fg {
        // The main prompt's pointer is uncoloured; a viewed agent colours it.
        return Err("TERMINAL_VIEW_NOT_MAIN");
    }
    let indent = col + 2;
    let footer = &lines[end + 1..];
    let all_ui = || footer.iter().chain(std::iter::once(&lines[p]));
    if all_ui().any(|l| {
        l.contains("-- INSERT --") || l.contains("-- NORMAL --") || l.contains("-- VISUAL --")
    }) {
        return Err("TERMINAL_VIM_MODE");
    }
    let help = footer.iter().any(|l| l.contains("/ for commands"))
        && footer.iter().any(|l| l.contains("@ for file paths"));
    if !help {
        let suspicious = footer.iter().filter(|l| !l.trim().is_empty()).count() > 4
            || footer.iter().any(|l| {
                let t = l.trim_start();
                t.starts_with('/')
                    || t.starts_with('@')
                    || t.starts_with('❯')
                    || l.contains("esc to")
                    || l.contains("Esc to")
                    || l.contains("enter to")
                    || l.contains("Enter to")
                    || l.contains('↑')
            });
        if suspicious {
            return Err("TERMINAL_OVERLAY_UNIDENTIFIED");
        }
    }
    // Box-drawing cells are one column wide.
    let width = rows[p - 1].len();
    let mut content: Vec<&[Cell]> = vec![];
    for (i, row) in rows.iter().enumerate().take(end).skip(p) {
        if i > p
            && row
                .iter()
                .take(indent)
                .any(|c| c.ch != ' ' && c.ch != '\u{a0}')
        {
            return Err(if strict {
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE"
            } else {
                "TERMINAL_TYPED_TEXT_MISMATCH"
            });
        }
        content.push(row.get(indent..).unwrap_or(&[]));
    }
    let visible = |c: &Cell| c.ch != ' ' && c.ch != '\u{a0}';
    let dim = content
        .iter()
        .flat_map(|r| r.iter())
        .filter(|c| visible(c) && !c.style.inverse)
        .filter(|c| c.style.dim)
        .count();
    let plain = content
        .iter()
        .flat_map(|r| r.iter())
        .filter(|c| visible(c) && (!c.style.dim || c.style.inverse))
        .count();
    let draft =
        if plain == 0 || (strict && dim > 0 && content.len() == 1 && placeholder(content[0])) {
            // Nothing typed, or only the dim placeholder (its first cell is the cursor).
            if content.len() > 1 {
                return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
            }
            let line = row_text(content[0]);
            if line.trim_start().starts_with("Message @") {
                return Err("TERMINAL_VIEW_NOT_MAIN");
            }
            String::new()
        } else {
            if strict && dim > 0 {
                // Typed text mixed with dim (ghost/suggestion) text.
                return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
            }
            let mut out = vec![];
            for row in &content {
                if strict && indent + columns(row) >= width.saturating_sub(1) {
                    // Reaches the right edge: a soft wrap cannot be told from a newline.
                    return Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE");
                }
                let s: String = row
                    .iter()
                    .filter(|c| strict || !c.style.dim || c.style.inverse)
                    .map(|c| if c.ch == '\u{a0}' { ' ' } else { c.ch })
                    .collect();
                out.push(s.trim_end().to_owned());
            }
            let draft = out.join("\n");
            if strict && (draft.contains("[Pasted text") || draft.contains("[Image")) {
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
    Ok(View::Main(draft))
}
/// A placeholder row: dim text, optionally with the inverse cursor cell first.
fn placeholder(row: &[Cell]) -> bool {
    let cells: Vec<&Cell> = row
        .iter()
        .filter(|c| c.ch != ' ' && c.ch != '\u{a0}')
        .collect();
    !cells.is_empty()
        && cells
            .iter()
            .enumerate()
            .all(|(i, c)| c.style.dim || (i == 0 && c.style.inverse))
}
pub(crate) fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

// ---------------------------------------------------------------- host

pub(crate) struct Host {
    session: String,
    pane: String,
    socket: Option<PathBuf>,
    /// HERDR's agent kind for this provider (`claude`, `gemini`).
    agent: &'static str,
}
impl Host {
    fn new(t: &Terminal) -> Result<Self> {
        Self::for_agent(t, "claude")
    }
    pub(crate) fn for_agent(t: &Terminal, agent: &'static str) -> Result<Self> {
        let (session, pane) = crate::terminal::target(&t.target)?;
        Ok(Self {
            session: session.into(),
            pane: pane.into(),
            socket: t.socket.clone(),
            agent,
        })
    }
    fn raw(&self, args: &[&str]) -> Result<Vec<u8>> {
        crate::terminal::raw_at(&self.session, args, self.socket.as_deref())
    }
    /// HERDR's agent status for this pane (`idle|working|blocked|done|unknown`),
    /// None when HERDR reports no agent of this kind there.
    pub(crate) fn agent_status(&self) -> Result<Option<String>> {
        let v =
            crate::terminal::call_at(&self.session, &["agent", "list"], self.socket.as_deref())?;
        Ok(v["agents"].as_array().and_then(|a| {
            a.iter()
                .find(|x| x["pane_id"] == self.pane.as_str() && x["agent"] == self.agent)
                .and_then(|x| x["agent_status"].as_str().map(str::to_owned))
        }))
    }
    pub(crate) fn screen(&self) -> Result<Vec<Vec<Cell>>> {
        let out = self.raw(&[
            "pane", "read", &self.pane, "--source", "visible", "--format", "ansi",
        ])?;
        Ok(cells(&String::from_utf8_lossy(&out)))
    }
    pub(crate) fn keys(&self, keys: &[&str]) -> Result<()> {
        let mut args = vec!["pane", "send-keys", self.pane.as_str()];
        args.extend_from_slice(keys);
        self.raw(&args).map(|_| ())
    }
    pub(crate) fn text(&self, s: &str) -> Result<()> {
        // HERDR 0.9.5 takes the text verbatim (leading hyphens included) and
        // writes it without a bracketed-paste wrapper.
        self.raw(&["pane", "send-text", &self.pane, s]).map(|_| ())
    }
    /// Type text as keystrokes: small chunks, ctrl+j (`chat:newline`) for
    /// newlines; never Enter.
    pub(crate) async fn type_text(&self, s: &str) -> Result<()> {
        for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
                self.keys(&["ctrl+j"])?;
                tokio::time::sleep(CHUNK_PAUSE).await;
            }
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(CHUNK) {
                self.text(&chunk.iter().collect::<String>())?;
                tokio::time::sleep(CHUNK_PAUSE).await;
            }
        }
        Ok(())
    }
    /// Kill the whole input with ctrl+k / ctrl+u pairs (each removes the rest
    /// of a row, or the newline next to the cursor).
    async fn clear(&self, text: &str) -> Result<()> {
        let rows = text
            .split('\n')
            .map(|l| l.chars().count() / 40 + 1)
            .sum::<usize>();
        let pairs = rows * 2 + 2;
        for _ in 0..pairs.min(200) {
            self.keys(&["ctrl+k", "ctrl+u"])?;
        }
        Ok(())
    }
    /// Read the screen until `want` holds or the wait ends; returns the last view.
    async fn until(
        &self,
        strict: bool,
        want: impl Fn(&Result<View, &'static str>) -> bool,
    ) -> Result<Result<View, &'static str>> {
        let deadline = Instant::now() + SCREEN_WAIT;
        loop {
            tokio::time::sleep(SETTLE).await;
            let v = classify(&self.screen()?, strict);
            if want(&v) || Instant::now() >= deadline {
                return Ok(v);
            }
        }
    }
}

// ---------------------------------------------------------------- admission

/// The verified HERDR attachment of a running session, or why there is none.
pub fn eligible(o: &claude::Observation) -> Result<Terminal, &'static str> {
    let a = o.attachment.as_ref().ok_or("TERMINAL_NOT_ATTACHED")?;
    if a.host != "herdr" || o.availability != "running" || Some(a.pid) != o.pid {
        return Err("TERMINAL_NOT_ATTACHED");
    }
    crate::terminal::target(&a.target).map_err(|_| "TERMINAL_NOT_ATTACHED")?;
    let socket = claude::environ(a.pid).and_then(|env| {
        env.into_iter()
            .find(|(k, _)| k == "HERDR_SOCKET_PATH")
            .map(|(_, v)| PathBuf::from(v))
    });
    Ok(Terminal {
        target: a.target.clone(),
        socket: socket.filter(|s| s.is_absolute()),
        pid: a.pid,
        process_start: a.process_start,
        transcript: o.transcript.clone(),
        ..Default::default()
    })
}
pub fn prepare(request: &Request, mut t: Terminal) -> Result<Terminal> {
    let (text, escaped, normalized) = deliverable(&request.text)?;
    t.delivered_text = text;
    t.escaped_prefix = escaped;
    t.normalized = normalized;
    Ok(t)
}

// ---------------------------------------------------------------- worker

pub(crate) fn reason(e: &anyhow::Error) -> String {
    let s = e.to_string();
    let code = s.split(':').next().unwrap_or("REJECTED");
    if code.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !code.is_empty() {
        code.to_owned()
    } else {
        "TERMINAL_HOST_UNAVAILABLE".to_owned()
    }
}
pub(crate) fn step(paths: &Paths, id: &str, s: &str, f: impl FnOnce(&mut Record)) -> Result<()> {
    change(paths, id, |r| {
        if let Some(t) = r.terminal.as_mut()
            && t.steps.len() < 64
        {
            t.steps.push(s.to_owned());
        }
        f(r);
        Ok(())
    })
}
pub(crate) fn reject(paths: &Paths, id: &str, code: &str) -> Result<()> {
    change(paths, id, |r| {
        finish(r, "rejected", Some(code));
        Ok(())
    })
}

/// Recheck the target before touching the pane: same catalog entry and live
/// process, still attached to the same HERDR pane, no hosted worker.
fn recheck(paths: &Paths, r: &Record) -> Result<Terminal> {
    let account =
        crate::messaging::headless::usable_account(paths, ProviderId::Claude, &r.account)?;
    let session = catalog::find(paths, &r.session_id)?;
    ensure!(session.conversation_id == r.conversation_id, "STALE_TARGET");
    hosted::ensure_not_hosted(paths, &r.conversation_id)?;
    let recorded = r.terminal.clone().context("STALE_TARGET")?;
    let o = claude::observe(
        &account.home,
        &r.conversation_id,
        session.attachment.as_ref(),
    );
    let now = eligible(&o).map_err(|code| anyhow::anyhow!("{code}"))?;
    ensure!(
        now.pid == recorded.pid
            && now.process_start == recorded.process_start
            && now.target == recorded.target,
        "STALE_TARGET: the attached Claude process or pane changed"
    );
    Ok(Terminal {
        transcript: now.transcript.or(recorded.transcript.clone()),
        ..recorded
    })
}

pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = lock_file(&worker_lock(paths, id)?)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let r = read(paths, id)?;
    if r.phase != "admitted" || r.kind != "terminal" || super::gemini::is_gemini(&r) {
        return Ok(());
    }
    if r.request.expires_at <= crate::daemon::now() {
        return change(paths, id, |r| {
            finish(r, "expired", Some("EXPIRED_BEFORE_DELIVERY"));
            Ok(())
        });
    }
    let t = match recheck(paths, &r) {
        Ok(t) => t,
        Err(e) => return abandon_unsent(paths, id, &reason(&e)),
    };
    // One delivery per pane at a time.
    private_dir(&dir(paths))?;
    let key: String = t
        .target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let pane_lock = lock_file(&dir(paths).join(format!("terminal-{key}.lock")))?;
    pane_lock.lock_exclusive()?;
    change(paths, id, |r| {
        r.terminal = Some(t.clone());
        Ok(())
    })?;
    let host = Host::new(&t)?;
    let outcome = deliver(paths, id, &host, &t).await;
    if let Err(e) = outcome {
        // A HERDR failure mid-procedure: never retried. Before Enter the
        // request is unsent; after it, the transcript decides. A draft whose
        // restore was not verified stays in the private journal.
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

async fn deliver(paths: &Paths, id: &str, host: &Host, t: &Terminal) -> Result<()> {
    // 1. HERDR agent state (read-only).
    match host.agent_status() {
        Ok(Some(s)) if s == "blocked" => return reject(paths, id, "TERMINAL_AGENT_BLOCKED"),
        Ok(Some(_)) => {}
        Ok(None) => return reject(paths, id, "TERMINAL_AGENT_UNKNOWN"),
        Err(_) => return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE"),
    }
    // 2. View: main prompt, or a view left with its own toggle.
    let Ok(screen) = host.screen() else {
        return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE");
    };
    let mut view = classify(&screen, true);
    for _ in 0..2 {
        let (key, name): (&[&str], &str) = match &view {
            Ok(View::Transcript) => (&["ctrl+o"], "transcript"),
            Ok(View::Help) => (&[], "help"),
            _ => break,
        };
        step(paths, id, &format!("leave_{name}"), |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.view_switched = Some(name.into());
            }
        })?;
        if key.is_empty() {
            host.text("?")?;
        } else {
            host.keys(key)?;
        }
        view = host
            .until(true, |v| matches!(v, Ok(View::Main(_))) || v.is_err())
            .await?;
    }
    let draft = match view {
        Ok(View::Main(d)) => d,
        Ok(_) => return reject(paths, id, "TERMINAL_VIEW_UNDETERMINED"),
        Err(code) => return reject(paths, id, code),
    };
    if draft.starts_with(['?', '!', '#']) || draft.contains('\t') {
        // Typing it back would toggle help or a mode instead of the character.
        return reject(paths, id, "TERMINAL_DRAFT_UNRESTORABLE");
    }
    // Re-check the agent just before the first destructive key.
    if matches!(host.agent_status(), Ok(Some(s)) if s == "blocked") {
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
        host.clear(&draft).await?;
        let v = host
            .until(true, |v| matches!(v, Ok(View::Main(d)) if d.is_empty()))
            .await?;
        if !matches!(&v, Ok(View::Main(d)) if d.is_empty()) {
            // The input is in an unknown state: never type over it.
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
    let typed = host
        .until(
            false,
            |v| matches!(v, Ok(View::Main(d)) if squash(d) == want),
        )
        .await?;
    let matches = matches!(&typed, Ok(View::Main(d)) if squash(d) == want);
    let blocked = matches!(host.agent_status(), Ok(Some(s)) if s == "blocked");
    if !matches || blocked {
        let code = if blocked {
            "TERMINAL_AGENT_BLOCKED"
        } else {
            "TERMINAL_TYPED_TEXT_MISMATCH"
        };
        step(paths, id, "withdraw", |_| {})?;
        host.clear(&t.delivered_text).await?;
        let cleared = host
            .until(true, |v| matches!(v, Ok(View::Main(d)) if d.is_empty()))
            .await?;
        if matches!(&cleared, Ok(View::Main(d)) if d.is_empty()) {
            restore(paths, id, host, &draft).await?;
        } else {
            change(paths, id, |r| {
                if let Some(t) = r.terminal.as_mut() {
                    t.draft_restored = Some(false);
                    t.restore_error = Some("withdraw_unverified_draft_kept_in_journal".into());
                }
                Ok(())
            })?;
        }
        return reject(paths, id, code);
    }
    // 5. Submit: journal first, then exactly one Enter.
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
        }
    })?;
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
    // 6. Put the draft back, unsubmitted.
    if !draft.is_empty() {
        let empty = host
            .until(true, |v| matches!(v, Ok(View::Main(d)) if d.is_empty()))
            .await?;
        if matches!(&empty, Ok(View::Main(d)) if d.is_empty()) {
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
    let v = host
        .until(true, |v| matches!(v, Ok(View::Main(d)) if d == draft))
        .await;
    let ok = typed.is_ok() && matches!(&v, Ok(Ok(View::Main(d))) if d == draft);
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

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
/// Milliseconds of an RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS[.fff]Z`).
pub(crate) fn timestamp_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let frac = s.get(19..s.len() - 1).unwrap_or("");
    let ms = if let Some(f) = frac.strip_prefix('.') {
        let digits: String = f.chars().take(3).collect();
        format!("{digits:0<3}").parse::<i64>().ok()?
    } else {
        0
    };
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 24 + hh) * 60 + mm) * 60 * 1000 + ss * 1000 + ms)
}
fn content_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let texts: Vec<&str> = a
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            (!texts.is_empty() && texts.len() == a.len()).then(|| texts.join(""))
        }
        _ => None,
    }
}
fn same(found: &str, typed: &str) -> bool {
    found == typed || found.trim() == typed.trim()
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Found {
    pub uuid: Option<String>,
    pub record: Option<String>,
    pub reply: Vec<String>,
    pub turn_ended: bool,
}

/// Find the human record that carries exactly the typed text, written at or
/// after `since_ms` and after `offset`; then collect assistant text until the
/// next real input. Only that record anchors a reply.
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
    for line in reader.split(b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(&line?) else {
            continue;
        };
        let fresh = v["timestamp"]
            .as_str()
            .and_then(timestamp_ms)
            .is_some_and(|t| t >= since_ms);
        let human = |o: &Value| o.is_null() || o["kind"] == "human";
        if out.uuid.is_none() {
            let queued = v["type"] == "attachment"
                && v["attachment"]["type"] == "queued_command"
                && human(&v["attachment"]["origin"])
                && content_text(&v["attachment"]["prompt"]).is_some_and(|s| same(&s, text));
            let user = v["type"] == "user"
                && v["isMeta"] != true
                && human(&v["origin"])
                && content_text(&v["message"]["content"]).is_some_and(|s| same(&s, text));
            if fresh && (queued || user) {
                out.uuid = v["uuid"].as_str().map(str::to_owned);
                out.record = Some(if queued { "queued_command" } else { "user" }.into());
                if out.uuid.is_none() {
                    out.record = None;
                }
            }
            continue;
        }
        match v["type"].as_str() {
            Some("assistant") => {
                let t = hosted::text_blocks(&v["message"]["content"]).join("");
                if !t.is_empty() {
                    out.reply.push(t);
                }
            }
            Some("user") => {
                let content = &v["message"]["content"];
                let tool_only = content
                    .as_array()
                    .is_some_and(|a| !a.is_empty() && a.iter().all(|b| b["type"] == "tool_result"));
                let echo = content_text(content).is_some_and(|s| same(&s, text));
                if !tool_only && v["isMeta"] != true && !echo {
                    out.turn_ended = true;
                    break;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Apply transcript evidence to a written terminal record (never regressed).
pub(crate) fn observe(paths: &Paths, id: &str, idle: bool) -> Result<()> {
    let r = read(paths, id)?;
    let Some(t) = r.terminal.clone() else {
        return Ok(());
    };
    if is_terminal(&r) || !r.facts.written {
        return Ok(());
    }
    let path = t.transcript.clone().or_else(|| {
        let account = claude::account(paths, &r.account).ok()?;
        claude::find_transcript(&account.home, &r.conversation_id)
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
        let Some(uuid) = found.uuid else {
            r.observation_error = Some("terminal_input_not_found".into());
            return Ok(());
        };
        r.observation_error = None;
        t.input_uuid = Some(uuid);
        t.input_record = found.record.clone();
        t.turn_ended |= found.turn_ended;
        let ended = t.turn_ended || (idle && !found.reply.is_empty());
        let queued = found.record.as_deref() == Some("queued_command");
        r.facts.input_recorded = true;
        r.facts.folded = queued;
        r.facts.shared_with_turn = queued;
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
            r.state = if queued && found.reply.is_empty() {
                "native_accepted".into()
            } else {
                "execution_started".into()
            };
        }
        Ok(())
    })
}

/// After a submit: wait for the human record (or report the send uncertain),
/// then follow the reply until the turn ends or the timeout. The turn ends at
/// the next real input in the transcript, or when HERDR has reported the agent
/// working and then idle for two seconds.
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
    let confirm = Duration::from_secs(timeout.min(CONFIRM_SECONDS));
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

/// Receipt fields of a terminal record.
pub fn receipt_fields(r: &Record, v: &mut Value) {
    let Some(t) = &r.terminal else {
        return;
    };
    v["correlation"] = json!({"kind":"transcript_record","input_uuid":t.input_uuid,"record_type":t.input_record,
        "anchor_offset":t.anchor_offset,"turn_ended":t.turn_ended});
    if t.input_uuid.is_none() {
        v["correlation"] = if r.facts.written {
            v["correlation"].clone()
        } else {
            Value::Null
        };
    }
    v["terminal_input"] = json!({"label":LABEL,"target":t.target,"escaped_prefix":t.escaped_prefix,
        "text_normalized":t.normalized,"view_switched":t.view_switched,
        "draft":{"present":t.draft_chars.map(|n| n > 0),"chars":t.draft_chars,"restored":t.draft_restored,
            "restore_error":t.restore_error},
        "steps":t.steps});
}

/// With the worker gone, a submit never matched in the transcript within the
/// confirmation window stays uncertain for good: `delivery_unknown`.
pub(crate) fn unconfirmed_after_exit(paths: &Paths, id: &str) -> Result<()> {
    change(paths, id, |r| {
        let Some(t) = r.terminal.as_ref() else {
            return Ok(());
        };
        let late = t
            .submitted_at_ms
            .is_some_and(|at| now_ms() - at > (CONFIRM_SECONDS as i64 + 5) * 1000);
        if !is_terminal(r) && r.facts.written && t.input_uuid.is_none() && late {
            finish(r, "delivery_unknown", Some("TERMINAL_INPUT_UNCONFIRMED"));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const RULE: &str = "────────────────────────────────────────";
    fn screen(prompt: &[&str], footer: &[&str]) -> Vec<Vec<Cell>> {
        let mut s = format!("● earlier output\n{RULE}\n");
        for l in prompt {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str(RULE);
        for l in footer {
            s.push('\n');
            s.push_str(l);
        }
        cells(&s)
    }
    #[test]
    fn payloads_are_escaped_or_refused_before_anything_is_typed() {
        assert_eq!(
            deliverable("hello").unwrap(),
            ("hello".into(), false, false)
        );
        assert_eq!(deliverable("/clear now.").unwrap().0, "\\/clear now.");
        assert_eq!(deliverable("!rm -rf x.").unwrap().0, "\\!rm -rf x.");
        assert_eq!(deliverable("# note").unwrap().0, "\\# note");
        assert_eq!(deliverable("?").unwrap().0, "\\?");
        assert_eq!(
            deliverable("  a\r\nb  ").unwrap(),
            ("a\nb".into(), false, true)
        );
        for bad in [
            "tab\there",
            "esc\u{1b}[A",
            "ends with \\",
            "see @src/main.rs",
            "run /compact",
            "   ",
        ] {
            assert!(deliverable(bad).is_err(), "{bad}");
        }
        assert!(deliverable(&"x".repeat(MAX_TEXT + 1)).is_err());
    }
    #[test]
    fn the_main_prompt_draft_is_read_exactly_or_refused() {
        // Empty prompt with the dim placeholder and the cursor on its first cell.
        let s = screen(
            &["❯\u{a0}\u{1b}[7mT\u{1b}[0m\u{1b}[2mry \"fix lint errors\"\u{1b}[0m"],
            &["  ? for shortcuts"],
        );
        assert_eq!(classify(&s, true), Ok(View::Main(String::new())));
        let s = screen(&["❯\u{a0}\u{1b}[7m \u{1b}[0m"], &[]);
        assert_eq!(classify(&s, true), Ok(View::Main(String::new())));
        let s = screen(
            &["❯\u{a0}half a thought", "  second line\u{1b}[7m \u{1b}[0m"],
            &["  ? for shortcuts"],
        );
        assert_eq!(
            classify(&s, true),
            Ok(View::Main("half a thought\nsecond line".into()))
        );
        for (prompt, code) in [
            (
                vec!["❯\u{a0}[Pasted text #1 +40 lines]"],
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
            ),
            (
                vec!["❯\u{a0}look [Image #1]"],
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
            ),
            // Typed text followed by a dim suggestion.
            (
                vec!["❯\u{a0}fix\u{1b}[2m the tests\u{1b}[0m"],
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
            ),
            // Reaches the right edge: may be soft-wrapped.
            (
                vec!["❯\u{a0}aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "  bb"],
                "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
            ),
            // A viewed subagent colours the pointer.
            (
                vec!["\u{1b}[38;5;5m❯\u{1b}[0m\u{a0}draft"],
                "TERMINAL_VIEW_NOT_MAIN",
            ),
            (
                vec!["❯\u{a0}\u{1b}[2mMessage @worker…\u{1b}[0m"],
                "TERMINAL_VIEW_NOT_MAIN",
            ),
        ] {
            assert_eq!(
                classify(&screen(&prompt, &[]), true),
                Err(code),
                "{prompt:?}"
            );
        }
        let tall: Vec<String> = (0..20).map(|i| format!("  line {i}")).collect();
        let mut prompt = vec!["❯\u{a0}first"];
        prompt.extend(tall.iter().map(String::as_str));
        assert_eq!(
            classify(&screen(&prompt, &[]), true),
            Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE")
        );
    }
    #[test]
    fn views_and_overlays_are_identified_or_refused() {
        let transcript = cells("● reply\n  Showing detailed transcript · ctrl+o to toggle");
        assert_eq!(classify(&transcript, true), Ok(View::Transcript));
        let help = screen(
            &["❯\u{a0}"],
            &[
                "  ! for shell mode    / for commands",
                "  @ for file paths   double tap esc to clear input",
            ],
        );
        assert_eq!(classify(&help, true), Ok(View::Help));
        let vim = screen(&["❯\u{a0}draft"], &["  -- NORMAL --"]);
        assert_eq!(classify(&vim, true), Err("TERMINAL_VIM_MODE"));
        let menu = screen(
            &["❯\u{a0}/mo"],
            &[
                "  /model     Set the AI model",
                "  /mcp       Manage MCP servers",
            ],
        );
        assert_eq!(classify(&menu, true), Err("TERMINAL_OVERLAY_UNIDENTIFIED"));
        let picker =
            cells("  Select model\n  ❯ 1. Default\n    2. Opus\n  Enter to confirm · Esc to exit");
        assert!(classify(&picker, true).is_err());
        let shell = screen(&["!\u{a0}ls"], &[]);
        assert_eq!(classify(&shell, true), Err("TERMINAL_SHELL_MODE"));
        assert_eq!(
            classify(&cells("plain shell $"), true),
            Err("TERMINAL_VIEW_UNDETERMINED")
        );
        // Typed text is verified leniently (wraps allowed, ghost text ignored).
        let wrapped = screen(
            &[
                "❯\u{a0}aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "  bb\u{1b}[2m ghost\u{1b}[0m",
            ],
            &[],
        );
        assert_eq!(
            classify(&wrapped, false),
            Ok(View::Main(format!("{}\nbb", "a".repeat(38))))
        );
    }
    #[test]
    fn the_typed_record_is_found_only_after_the_submit() {
        assert_eq!(timestamp_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(
            timestamp_ms("2026-10-29T00:00:00Z"),
            Some(1_793_232_000_000)
        );
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.jsonl");
        let lines = [
            json!({"type":"user","uuid":"old","timestamp":"2026-10-29T00:00:00.000Z","message":{"content":"status?"}}),
            json!({"type":"attachment","uuid":"peer","timestamp":"2026-10-29T00:00:10.000Z","attachment":{"type":"queued_command","prompt":"status?","origin":{"kind":"peer"}}}),
            json!({"type":"attachment","uuid":"mine","timestamp":"2026-10-29T00:00:11.000Z","attachment":{"type":"queued_command","prompt":"status?","origin":{"kind":"human"}}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}),
            json!({"type":"user","uuid":"echo","timestamp":"2026-10-29T00:00:12.000Z","message":{"content":"status?"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}),
            json!({"type":"user","uuid":"next","message":{"content":"another"}}),
        ];
        fs::write(
            &p,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        let since = timestamp_ms("2026-10-29T00:00:05.000Z").unwrap();
        let f = find(&p, 0, "status?", since).unwrap();
        assert_eq!(f.uuid.as_deref(), Some("mine"));
        assert_eq!(f.record.as_deref(), Some("queued_command"));
        assert_eq!(f.reply, ["working on it", "done"]);
        assert!(f.turn_ended);
        // Nothing after the submit: no anchor, no reply.
        let f = find(
            &p,
            0,
            "status?",
            timestamp_ms("2026-10-29T01:00:00Z").unwrap(),
        )
        .unwrap();
        assert_eq!(f, Found::default());
    }
}
