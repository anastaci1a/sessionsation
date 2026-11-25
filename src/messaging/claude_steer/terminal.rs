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
pub(crate) use super::typing::{CONFIRM_SECONDS, Host};
use super::{
    Record, Request, abandon_unsent, all, change, dir, finish, is_terminal, read,
    typing::{self, ClearKeys, Draft, Leave, Prefix, Screen, ScreenProfile, Typing, is_main},
    worker_lock,
};
use crate::{
    catalog,
    paths::{Paths, lock_file, private_dir},
    provider::{ProviderId, claude, claude::hosted},
};
use anyhow::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Read, Seek},
    path::{Path, PathBuf},
};

pub const SEMANTICS: &str = "terminal_input";
pub const LABEL: &str = "Typed into the session's HERDR terminal as the user's own input: Claude records it as a human message (queued mid-turn, or a new turn when idle)";
/// Longest text typed into a terminal (the prompt must stay verifiable on screen).
pub const MAX_TEXT: usize = 8192;
/// Rows a draft may occupy and still be read back exactly.
const MAX_DRAFT_ROWS: usize = 12;
/// Rows a typed message may occupy and still be verified.
const MAX_TYPED_ROWS: usize = 200;
const MAX_SCAN: u64 = 16 * 1024 * 1024;

pub const DISABLED: &str = "TERMINAL_INPUT_DISABLED: terminal input is turned off by the operator (config terminal_input_enabled)";

/// The operator kill switch (`terminal_input_enabled`, default on). An
/// unreadable configuration counts as off.
pub fn enabled(paths: &Paths) -> bool {
    crate::store::load_config(paths).is_ok_and(|c| c.terminal_input_enabled)
}

pub const REASONS: &[&str] = &[
    "TERMINAL_NOT_ATTACHED",
    "TERMINAL_INPUT_DISABLED",
    "TERMINAL_QUEUE_UNSUPPORTED",
    "TERMINAL_INTERRUPT_UNSUPPORTED",
    "TERMINAL_PAYLOAD_UNSUPPORTED",
    "TERMINAL_TEXT_TOO_LONG",
    "TERMINAL_HOST_UNAVAILABLE",
    "TERMINAL_AGENT_UNKNOWN",
    "TERMINAL_AGENT_BLOCKED",
    "TERMINAL_PANE_IS_SHELL",
    "TERMINAL_PROVIDER_EXITED",
    "TERMINAL_VIEW_NOT_MAIN",
    "TERMINAL_VIEW_UNDETERMINED",
    "TERMINAL_OVERLAY_UNIDENTIFIED",
    "TERMINAL_VIM_MODE",
    "TERMINAL_SHELL_MODE",
    "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
    "TERMINAL_DRAFT_UNRESTORABLE",
    "TERMINAL_CLEAR_UNVERIFIED",
    "TERMINAL_DRAFT_CHANGED",
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
    /// What the input box held when the procedure ended after the clear:
    /// `draft` (restored) | `empty` | `message` | `other` | `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_in_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_chars: Option<usize>,
    /// Why a screen was refused: the layout around the input box (no draft text), for diagnosis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_layout: Option<Value>,
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
    /// Antigravity CLI sessions only (see [`super::antigravity`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity: Option<super::antigravity::Facts>,
}

// ---------------------------------------------------------------- payload

/// The text as typed: CRLF normalized, trimmed, a leading `/`, `!`, `#` or
/// `?` escaped with a backslash so it stays plain text (command, shell,
/// memory and help triggers). Refuses text the prompt cannot take verbatim.
pub fn deliverable(text: &str) -> Result<(String, bool, bool)> {
    typing::deliverable::<Claude>(text)
}

/// Claude Code 2.1.312's terminal for the typing engine: the box under a rule with `❯` in
/// column 0 ([`classify`]), ctrl+k / ctrl+u clearing, the transcript view left with ctrl+o
/// and the shortcut help with `?`, typed in verified segments, one Enter, and what the box
/// holds reported whenever a restore is not verified.
pub(crate) struct Claude;
impl ScreenProfile for Claude {
    const AGENT: &'static str = "claude";
    const PROVIDER: ProviderId = ProviderId::Claude;
    const PROCESS: &'static str = "Claude";
    const MAX_TEXT: usize = MAX_TEXT;
    const MAX_LINES: Option<usize> = None;
    const PREFIX: Prefix = Prefix::Escape(&['/', '!', '#', '?']);
    /// ctrl+k / ctrl+u pairs (kill to end / start of the line, each also removing an
    /// adjacent newline); ctrl+l is only a redraw in 2.1.312.
    const CLEAR: ClearKeys = ClearKeys {
        first: &[],
        round: &["ctrl+k", "ctrl+u"],
        row_chars: 40,
        rounds_per_row: 2,
        extra: 2,
        max: 600,
    };
    const VIEW_SWITCHES: usize = 2;
    const TYPING: Typing = Typing::Checked { reread: false };
    const DRAFT: Draft = Draft::Reported;
    const ONE_BLOCKED_READ: bool = true;
    const RESTORE_ERROR_NAMES_HOST: bool = true;
    const REREAD_DRAFT: bool = true;
    const REFUSAL_LAYOUT: bool = true;
    const MODE_GATED: bool = false;
    fn too_long() -> String {
        format!("TERMINAL_TEXT_TOO_LONG: terminal input takes at most {MAX_TEXT} bytes")
    }
    fn classify(rows: &[Vec<Cell>], strict: bool) -> Screen {
        classify(rows, strict).map(|v| match v {
            View::Main(draft) => typing::View::Main { draft, busy: false },
            View::Transcript => typing::View::Transcript,
            View::Help => typing::View::Help,
        })
    }
    fn leave(view: &typing::View) -> Option<(&'static str, Leave)> {
        match view {
            typing::View::Transcript => Some(("transcript", Leave::Keys(&["ctrl+o"]))),
            typing::View::Help => Some(("help", Leave::Text("?"))),
            typing::View::Main { .. } => None,
        }
    }
    fn restorable(draft: &str) -> bool {
        restorable(draft)
    }
    fn observe_process(
        home: &Path,
        conversation: &str,
        attachment: Option<&catalog::Attachment>,
    ) -> claude::Observation {
        claude::observe(home, conversation, attachment)
    }
    /// No hosted worker serves the conversation.
    fn before_observe(paths: &Paths, r: &Record) -> Result<()> {
        hosted::ensure_not_hosted(paths, &r.conversation_id)
    }
    fn apply_transcript(paths: &Paths, id: &str, idle: bool) -> Result<()> {
        observe(paths, id, idle)
    }
}

// ---------------------------------------------------------------- screen

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Style {
    pub dim: bool,
    pub fg: bool,
    pub inverse: bool,
    /// A background colour is set (SGR 40-47, 48, 100-107).
    pub bg: bool,
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
            40..=47 | 100..=107 => style.bg = true,
            49 => style.bg = false,
            48 => {
                style.bg = true;
                i += if codes.get(i + 1) == Some(&5) { 2 } else { 4 };
            }
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
pub(crate) fn is_rule(row: &[Cell]) -> bool {
    matches!(row.first().map(|c| c.ch), Some('─' | '╭' | '╰'))
        && (row.iter().filter(|c| c.ch == '─').count() * 2 > row.len() || labelled_rule(row))
}
/// A top rule carrying the session name (`──── name ─`, a resumed or named
/// session) is mostly text: the real post-resume screen has 8 dash cells in 58.
fn labelled_rule(row: &[Cell]) -> bool {
    row.iter().take_while(|c| c.ch == '─').count() >= 3
        && row
            .iter()
            .find(|c| c.ch != '─')
            .is_some_and(|c| c.ch == ' ')
}
/// The top rule of a viewed agent's box carries the agent's name as a pill (a
/// label on a coloured background). The main view's rule is plain, or (a
/// named or attached background session) carries the session name as plain
/// text on the default background.
fn agent_pill(row: &[Cell]) -> bool {
    row.iter().any(|c| c.style.bg && c.ch != '─')
}
pub(crate) fn indent_of(line: &str) -> (usize, &str) {
    let rest = line.trim_start_matches([' ', '\u{a0}']);
    (line.chars().count() - rest.chars().count(), rest)
}
/// A completion or command menu drawn directly above the input box (Claude
/// Code 2.1.312): the selected entry is `  ❯ /model …`, the others `    /mobile …`
/// or `    * agent …`. Other rows there (a spinner tip `  ⎿  …`, a
/// right-aligned hint) are not menus.
pub(crate) fn menu_row(line: &str) -> bool {
    let (indent, rest) = indent_of(line);
    (indent == 2 && rest.starts_with('❯'))
        || (indent == 4 && rest.starts_with(['/', '*', '+', '@', '#', '!']))
}
/// An agent-list row under the footer (`  ● main`, `  ◯ general-purpose …`),
/// shown while background agents exist; not an overlay by itself.
fn agent_row(line: &str) -> bool {
    let (indent, rest) = indent_of(line);
    indent == 2 && rest.starts_with(['●', '◯'])
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
///
/// Markers (real Claude Code 2.1.312 screens under
/// `tests/fixtures/claude-screens/`): the input box is the last `❯` in column 0
/// directly under a rule, closed by the next rule. The pointer's colour is NOT
/// a marker: it is grey while a turn runs and default when idle, in the main
/// view and in a viewed agent alike. A viewed agent is told by its name as a
/// pill (coloured background) in the top rule, a `Message @name…` placeholder
/// or `◯ main` in the agent list. A background session's own name in that rule
/// is plain text and stays the main view.
pub(crate) fn classify(rows: &[Vec<Cell>], strict: bool) -> Result<View, &'static str> {
    let lines: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    if lines
        .iter()
        .any(|l| l.contains("Showing detailed transcript"))
    {
        return Ok(View::Transcript);
    }
    // Column 0 only: dialogs indent their `❯` (` ❯ 1. Yes`, `  ❯ /model`).
    let pointer = |l: &str| {
        let mut c = l.chars();
        c.next() == Some('❯') && matches!(c.next(), None | Some(' ' | '\u{a0}'))
    };
    // The box: a pointer row right under a rule (the agent list under the
    // footer marks its selected row with `❯` too).
    let Some(p) = (1..lines.len())
        .rev()
        .find(|&i| pointer(&lines[i]) && is_rule(&rows[i - 1]))
    else {
        let shell = lines.iter().any(|l| l.trim_start().starts_with("!\u{a0}"));
        return Err(if shell {
            "TERMINAL_SHELL_MODE"
        } else {
            "TERMINAL_VIEW_UNDETERMINED"
        });
    };
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
    if agent_pill(&rows[p - 1]) {
        // A labelled pill in the top rule: the box of a viewed agent.
        return Err("TERMINAL_VIEW_NOT_MAIN");
    }
    let indent = 2;
    let footer = &lines[end + 1..];
    if footer.iter().any(|l| {
        let (_, rest) = indent_of(l);
        rest.trim_start_matches('❯')
            .trim_start()
            .starts_with("◯ main")
    }) {
        // The agent list marks main as not shown.
        return Err("TERMINAL_VIEW_NOT_MAIN");
    }
    let all_ui = || footer.iter().chain(std::iter::once(&lines[p]));
    if all_ui().any(|l| {
        l.contains("-- INSERT --") || l.contains("-- NORMAL --") || l.contains("-- VISUAL --")
    }) {
        return Err("TERMINAL_VIM_MODE");
    }
    if p >= 2 && menu_row(&lines[p - 2]) {
        return Err("TERMINAL_OVERLAY_UNIDENTIFIED");
    }
    if rows[end + 1..]
        .iter()
        .flatten()
        .any(|c| c.style.inverse && c.ch != ' ')
    {
        // A highlighted footer item: keyboard focus left the input box.
        return Err("TERMINAL_OVERLAY_UNIDENTIFIED");
    }
    let help = footer.iter().any(|l| l.contains("/ for commands"))
        && footer.iter().any(|l| l.contains("@ for file paths"));
    if !help {
        let suspicious = footer
            .iter()
            .filter(|l| !l.trim().is_empty() && !agent_row(l))
            .count()
            > 4
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

// ---------------------------------------------------------------- admission

/// The verified HERDR attachment of a running session, or why there is none.
///
/// The pane runs either the Claude process itself, or, for a background
/// (`bg`) session, a `claude attach <job>` client of it or the interactive
/// process parked into it (registry `parkedJobId`): keys typed there reach the
/// background session and land in its transcript. The client is
/// accepted only when it is the exact recorded process (PID and start time)
/// and HERDR reports it as the pane's only foreground process.
pub fn eligible(o: &claude::Observation) -> Result<Terminal, &'static str> {
    let a = o.attachment.as_ref().ok_or("TERMINAL_NOT_ATTACHED")?;
    if a.host != "herdr" || o.availability != "running" {
        return Err("TERMINAL_NOT_ATTACHED");
    }
    crate::terminal::target(&a.target).map_err(|_| "TERMINAL_NOT_ATTACHED")?;
    let socket = claude::environ(a.pid)
        .and_then(|env| {
            env.into_iter()
                .find(|(k, _)| k == "HERDR_SOCKET_PATH")
                .map(|(_, v)| PathBuf::from(v))
        })
        .filter(|s| s.is_absolute());
    if Some(a.pid) != o.pid {
        if !attach_client(o, a) {
            return Err("TERMINAL_NOT_ATTACHED");
        }
        match crate::terminal::state_at(&a.target, socket.as_deref()) {
            Ok((false, pid)) if pid == a.pid => {}
            _ => return Err("TERMINAL_NOT_ATTACHED"),
        }
    }
    Ok(Terminal {
        target: a.target.clone(),
        socket,
        pid: a.pid,
        process_start: a.process_start,
        transcript: o.transcript.clone(),
        ..Default::default()
    })
}
/// The attachment is a `claude attach` client of this running background
/// session, or the interactive process parked into it.
fn attach_client(o: &claude::Observation, a: &crate::catalog::Attachment) -> bool {
    o.native_kind.as_deref() == Some("bg")
        && o.job_id
            .as_deref()
            .is_some_and(|job| claude::is_terminal_client(a.pid, a.process_start, job))
}
/// Row-level availability (no HERDR call): `None` when terminal input applies.
/// A background session's attach client is checked by its exact process; the
/// pane itself is verified again at admission and before the first key.
pub fn row_reason(value: &Value) -> Option<&'static str> {
    let a = &value["attachment"];
    let pid = a["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
    let start = a["process_start"].as_u64();
    let target_ok = a["target"]
        .as_str()
        .is_some_and(|t| crate::terminal::target(t).is_ok());
    let direct = a["pid"] == value["pid"] && a["process_start"] == value["process_start"];
    let bg_client = || {
        value["native"]["kind"] == "bg"
            && value["native"]["job_id"]
                .as_str()
                .zip(pid.zip(start))
                .is_some_and(|(job, (pid, start))| claude::is_terminal_client(pid, start, job))
    };
    let verified = a["host"] == "herdr" && pid.is_some() && target_ok && (direct || bg_client());
    (!verified).then_some("TERMINAL_NOT_ATTACHED")
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
/// What a refused screen looked like around the input box, without any draft text: the rows
/// between the last two rules (count, longest row), and the footer rows below the box (UI
/// chrome: key hints, status line, agent list), each cut to 120 characters.
pub(crate) fn layout(rows: &[Vec<Cell>]) -> Value {
    let lines: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    let rules: Vec<usize> = (0..rows.len()).filter(|&i| is_rule(&rows[i])).collect();
    let (box_rows, footer_from) = match rules.as_slice() {
        [.., a, b] => (b - a - 1, b + 1),
        [b] => (0, b + 1),
        _ => (0, rows.len()),
    };
    let footer: Vec<String> = lines[footer_from.min(lines.len())..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .map(|l| l.trim_end().chars().take(120).collect())
        .collect();
    json!({"screen_rows":rows.len(),"rules":rules.len(),"box_rows":box_rows,"max_draft_rows":MAX_DRAFT_ROWS,"footer":footer})
}
pub(crate) fn reject_screen(paths: &Paths, id: &str, code: &str, rows: &[Vec<Cell>]) -> Result<()> {
    let layout = layout(rows);
    change(paths, id, |r| {
        if let Some(t) = r.terminal.as_mut() {
            t.refusal_layout = Some(layout);
        }
        finish(r, "rejected", Some(code));
        Ok(())
    })
}
pub(crate) fn reject(paths: &Paths, id: &str, code: &str) -> Result<()> {
    change(paths, id, |r| {
        finish(r, "rejected", Some(code));
        Ok(())
    })
}

pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = lock_file(&worker_lock(paths, id)?)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let r = read(paths, id)?;
    if orphaned(&r) {
        return cleanup_orphan(paths, id, &r).await;
    }
    if r.phase != "admitted"
        || r.kind != "terminal"
        || super::gemini::is_gemini(&r)
        || super::antigravity::is_antigravity(&r)
    {
        return Ok(());
    }
    if r.request.expires_at <= crate::daemon::now() {
        return change(paths, id, |r| {
            finish(r, "expired", Some("EXPIRED_BEFORE_DELIVERY"));
            Ok(())
        });
    }
    let t = match typing::recheck::<Claude>(paths, &r) {
        Ok((t, _, _)) => t,
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
    let host = Host::for_agent(&t, Claude::AGENT)?;
    // A dead worker's typed message or taken-away draft must not poison this delivery.
    withdraw_orphans(paths, id, &host, &t.target).await;
    let outcome = typing::deliver(&Claude, paths, id, &host, &t).await;
    if let Err(e) = outcome {
        typing::worker_failed(paths, id, &e)?;
    }
    drop(pane_lock);
    typing::observe_loop::<Claude>(paths, id, &host).await
}

/// A draft that typing reproduces as text: not a help/shell/memory trigger,
/// a command or a completion (`/`, a last token starting with `@`), no tab.
pub(crate) fn restorable(draft: &str) -> bool {
    let last = draft.split_whitespace().last().unwrap_or_default();
    !draft.starts_with(['?', '!', '#', '/']) && !draft.contains('\t') && !last.starts_with('@')
}

/// A terminal request whose worker is gone after the clear and before Enter: the typed
/// message may still sit in the input box and the user's draft is not restored. Never
/// delivered; this is cleaned up (and then ends `rejected`), not reported as sent.
pub(crate) fn orphaned(r: &Record) -> bool {
    r.kind == "terminal"
        && r.phase == "dispatching"
        && r.provider.is_none()
        && r.terminal.as_ref().is_some_and(|t| {
            t.steps.iter().any(|s| s == "clear")
                && !t.steps.iter().any(|s| s == "submit")
                && t.draft_restored.is_none()
        })
}

/// Whether a cleanup worker was already started for an orphaned request.
pub(crate) fn cleanup_started(r: &Record) -> bool {
    r.terminal
        .as_ref()
        .is_some_and(|t| t.steps.iter().any(|s| s == "orphan_cleanup"))
}

/// A request that ends unsent after its worker died past the clear: what the input box holds
/// is unknown (nothing could be checked), and the journaled draft stays private.
pub(crate) fn unsent_left_in_input(paths: &Paths, id: &str) -> Result<()> {
    change(paths, id, |r| {
        if orphaned(r)
            && let Some(t) = r.terminal.as_mut()
            && t.left_in_input.is_none()
        {
            t.left_in_input = Some("unknown".into());
            t.restore_error
                .get_or_insert_with(|| "worker_exited_after_clear".into());
        }
        Ok(())
    })
}

/// Take the typed text of a dead worker out of the pane and put its draft back, then end
/// the request `rejected`. The pane lock is held by the caller. Best effort and never
/// sends Enter: a box that holds anything but the message (or nothing) is left as it is.
async fn withdraw_orphan(paths: &Paths, id: &str, host: &Host) {
    let Ok(r) = read(paths, id) else {
        return;
    };
    if !orphaned(&r) {
        return;
    }
    let Some(t) = r.terminal.clone() else {
        return;
    };
    let draft = t.draft.clone().unwrap_or_default();
    let message = t.delivered_text.clone();
    let typed = t.steps.iter().any(|s| s == "type");
    match host.screen().map(|rows| Claude::classify(&rows, false)) {
        Ok(v) if typed && is_main(&v, |d| typing::typed_remains(d, &message)) => {
            let ok = typing::clear::<Claude>(host, &message).await.is_ok();
            let v = if ok {
                typing::until::<Claude>(host, true, |v| is_main(v, str::is_empty))
                    .await
                    .unwrap_or(Err("TERMINAL_HOST_UNAVAILABLE"))
            } else {
                Err("TERMINAL_HOST_UNAVAILABLE")
            };
            if is_main(&v, str::is_empty) {
                let _ = typing::restore::<Claude>(paths, id, host, &draft, &message).await;
            } else {
                let _ = typing::left(
                    paths,
                    id,
                    "orphan_withdraw_unverified",
                    &v,
                    &draft,
                    &message,
                );
            }
        }
        Ok(v) if is_main(&v, str::is_empty) => {
            let _ = typing::restore::<Claude>(paths, id, host, &draft, &message).await;
        }
        Ok(v) => {
            let _ = typing::left(paths, id, "orphan_input_not_ours", &v, &draft, &message);
        }
        Err(_) => {
            let _ = typing::left(
                paths,
                id,
                "orphan_host_unavailable",
                &Err("TERMINAL_HOST_UNAVAILABLE"),
                &draft,
                &message,
            );
        }
    }
    let _ = change(paths, id, |r| {
        finish(r, "rejected", Some("TERMINAL_WORKER_EXITED_BEFORE_SUBMIT"));
        Ok(())
    });
}

/// Orphans of other requests on the same pane (see [`orphaned`]) are withdrawn before this
/// delivery reads the pane: their text would otherwise read as a draft nothing can restore.
async fn withdraw_orphans(paths: &Paths, id: &str, host: &Host, target: &str) {
    for o in all(paths) {
        let oid = o.request.message_id.clone();
        if oid == id
            || !orphaned(&o)
            || o.terminal.as_ref().is_none_or(|t| t.target != target)
            || !worker_lock(paths, &oid)
                .and_then(|p| crate::background::worker_stopped(&p))
                .unwrap_or(false)
        {
            continue;
        }
        withdraw_orphan(paths, &oid, host).await;
    }
}

/// Worker entry for an orphaned request found by a receipt read.
async fn cleanup_orphan(paths: &Paths, id: &str, r: &Record) -> Result<()> {
    let t = match typing::recheck::<Claude>(paths, r) {
        Ok((t, _, _)) => t,
        Err(_) => {
            // The target is gone: nothing can be withdrawn; say what is unknown.
            return change(paths, id, |r| {
                if let Some(t) = r.terminal.as_mut() {
                    t.left_in_input = Some("unknown".into());
                    t.restore_error
                        .get_or_insert_with(|| "orphan_target_unavailable".into());
                }
                finish(r, "rejected", Some("TERMINAL_WORKER_EXITED_BEFORE_SUBMIT"));
                Ok(())
            });
        }
    };
    private_dir(&dir(paths))?;
    let key: String = t
        .target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let pane_lock = lock_file(&dir(paths).join(format!("terminal-{key}.lock")))?;
    pane_lock.lock_exclusive()?;
    let host = Host::for_agent(&t, Claude::AGENT)?;
    withdraw_orphan(paths, id, &host).await;
    Ok(())
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
            "restore_error":t.restore_error,"left_in_input":t.left_in_input,"left_chars":t.left_chars},
        "steps":t.steps,"refusal_layout":t.refusal_layout});
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
mod tests;
