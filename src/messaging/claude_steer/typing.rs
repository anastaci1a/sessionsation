//! The terminal typing engine (step S8): one procedure for typing a message into the
//! HERDR pane of an interactive Claude Code, Gemini CLI or Antigravity CLI session as the
//! user's own input, parameterized by a per-provider [`ScreenProfile`].
//!
//! Procedure (one pane at a time, journaled before every write; see [`deliver`]):
//! 1. HERDR must report the provider's agent on the pane, not `blocked`, and the pane must
//!    run the exact provider process (contract 1.63.0), not a bare shell.
//! 2. The screen must show the main prompt with no overlay; views the profile can leave
//!    with their own toggle are left first.
//! 3. The user's draft is read from the screen and must be reconstructable exactly and
//!    restorable by typing; mode gates (Gemini's model steering, agy's queuedMessages) are
//!    checked against the busy state.
//! 4. The draft is journaled and cleared with the profile's clearing keys, never Esc,
//!    Ctrl+C or Ctrl+D.
//! 5. The text is typed in chunks with ctrl+j between lines and verified on screen; a
//!    mismatch withdraws it and puts the draft back.
//! 6. Exactly one submit key goes out, after a last provider check.
//! 7. The draft is typed back (not submitted) and verified.
//!
//! The profile supplies what differs: the screen classifier (prompt glyphs, input box
//! geometry, busy markers), limits, escape rules, clearing and submit keys, the views it
//! leaves, how the draft is accounted for when a restore cannot be verified, and the
//! transcript anchoring.
use super::{
    Record, change, finish, is_terminal, read,
    terminal::{
        self, Cell, Terminal, cells, now_ms, reason, reject, reject_screen, row_text, squash, step,
    },
};
use crate::{catalog, paths::Paths, provider::ProviderId};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Characters per `pane send-text` call.
const CHUNK: usize = 32;
/// Non-blank characters typed between two screen checks.
pub(crate) const SEGMENT: usize = 80;
const CHUNK_PAUSE: Duration = Duration::from_millis(15);
pub(crate) const SETTLE: Duration = Duration::from_millis(120);
pub(crate) const SCREEN_WAIT: Duration = Duration::from_secs(3);
/// Window in which the typed input must appear in the transcript.
pub(crate) const CONFIRM_SECONDS: u64 = 60;

// ---------------------------------------------------------------- profile

/// A screen as the engine sees it, mapped from the profile's own classifier.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum View {
    /// The main prompt with no overlay: the draft (empty when none), and whether a
    /// running turn is shown (always false for Claude, which does not tell).
    Main { draft: String, busy: bool },
    /// Claude's detailed transcript view.
    Transcript,
    /// The shortcut help.
    Help,
}
pub(crate) type Screen = Result<View, &'static str>;

pub(crate) fn is_main(v: &Screen, f: impl Fn(&str) -> bool) -> bool {
    matches!(v, Ok(View::Main { draft, .. }) if f(draft))
}
fn is_empty(v: &Screen) -> bool {
    is_main(v, str::is_empty)
}

/// What a leading command character gets.
pub(crate) enum Prefix {
    /// A backslash before it keeps it plain text.
    Escape(&'static [char]),
    /// No verified escape: refused with this text.
    Refuse(&'static [char], &'static str),
}

/// The keys that clear the input box: `first` once, then `round` per round. Rounds:
/// `rounds_per_row` per row (a line of `row_chars` characters, rounded up) plus `extra`,
/// at most `max`.
pub(crate) struct ClearKeys {
    pub first: &'static [&'static str],
    pub round: &'static [&'static str],
    pub row_chars: usize,
    pub rounds_per_row: usize,
    pub extra: usize,
    pub max: usize,
}

/// How a view is left: keys, or text typed on the empty prompt.
pub(crate) enum Leave {
    Keys(&'static [&'static str]),
    Text(&'static str),
}

/// How the typed message is verified.
pub(crate) enum Typing {
    /// Typed in segments, each checked as a window of the box ([`Host::type_checked`]);
    /// `reread`: the box is read once more afterwards and must still be the main prompt.
    Checked { reread: bool },
}

/// How the draft is accounted for when its restore cannot be verified.
#[derive(PartialEq)]
pub(crate) enum Draft {
    /// Claude: what the input box holds is reported (`left_in_input`, `left_chars`); a HERDR
    /// failure past the clear is salvaged once.
    Reported,
    /// Gemini and agy: the draft stays in the private journal with a `restore_error`.
    Kept,
}

/// The submit key and what it is expected to do.
pub(crate) struct Key {
    pub key: &'static str,
    pub expected: Option<&'static str>,
}

/// Everything the engine needs to know about one provider's terminal. Constants are the
/// provider's screen facts; `&self` methods read the per-delivery state (Gemini's model
/// steering, agy's queuedMessages).
pub(crate) trait ScreenProfile {
    /// HERDR's agent kind for the pane.
    const AGENT: &'static str;
    const PROVIDER: ProviderId;
    /// The process named in a STALE_TARGET refusal.
    const PROCESS: &'static str;
    const MAX_TEXT: usize;
    const MAX_LINES: Option<usize>;
    const PREFIX: Prefix;
    const CLEAR: ClearKeys;
    /// Views left in a row before the main prompt must show.
    const VIEW_SWITCHES: usize;
    const TYPING: Typing;
    const DRAFT: Draft;
    /// After typing, the agent state is read once for both the check and the refusal code
    /// (Claude), not once more for the code.
    const ONE_BLOCKED_READ: bool;
    /// A restore whose screen read failed names the HERDR code in its error
    /// (`restore_unverified:TERMINAL_HOST_UNAVAILABLE`), not just `restore_unverified`.
    const RESTORE_ERROR_NAMES_HOST: bool;
    /// The draft is read again right before the first clearing key.
    const REREAD_DRAFT: bool;
    /// A refused screen's layout is kept in the journal.
    const REFUSAL_LAYOUT: bool;
    /// The busy state gates the mode and picks the submit key ([`Self::refusal`],
    /// [`Self::key`]); it costs an agent-status read.
    const MODE_GATED: bool;

    fn too_long() -> String;
    fn classify(rows: &[Vec<Cell>], strict: bool) -> Screen;
    /// The view's name and how it is left, for a view the profile leaves.
    fn leave(_view: &View) -> Option<(&'static str, Leave)> {
        None
    }
    /// A draft typing reproduces as text (not a trigger, command or completion).
    fn restorable(draft: &str) -> bool;
    fn observe_process(
        home: &Path,
        conversation: &str,
        attachment: Option<&catalog::Attachment>,
    ) -> crate::provider::claude::Observation;
    /// Checked after the catalog entry, before the process (Claude: no hosted worker).
    fn before_observe(_paths: &Paths, _r: &Record) -> Result<()> {
        Ok(())
    }
    /// The refusal of the request's mode on a session that is (not) busy, before typing.
    fn refusal(&self, _mode: &str, _busy: bool) -> Option<&'static str> {
        None
    }
    /// The submit key for the mode once the text is typed, or the refusal.
    fn key(&self, _mode: &str, _busy: bool) -> Result<Key, &'static str> {
        Ok(Key {
            key: "enter",
            expected: None,
        })
    }
    /// The transcript offset the reply is anchored after.
    fn anchor(&self, _paths: &Paths, _id: &str, t: &Terminal) -> Result<Option<u64>> {
        Ok(t.transcript
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len()))
    }
    /// Provider facts journaled with the submit step.
    fn submit_facts(&self, _t: &mut Terminal, _busy: bool, _key: &Key) {}
    /// What the screen showed right after a busy submit (`queued`, a steering hint...).
    fn evidence(&self, _lines: &[String], _text: &str) -> Option<&'static str> {
        None
    }
    fn record_evidence(_t: &mut Terminal, _evidence: &str) {}
    /// How long a submit may go unrecorded.
    fn confirm_window(r: &Record) -> Duration {
        let timeout = r
            .request
            .timeout_seconds
            .unwrap_or(super::DEFAULT_TIMEOUT_SECONDS);
        Duration::from_secs(timeout.min(CONFIRM_SECONDS))
    }
    /// Apply transcript evidence to the written record.
    fn apply_transcript(paths: &Paths, id: &str, idle: bool) -> Result<()>;
}

// ---------------------------------------------------------------- payload

/// The text as typed: CRLF normalized and trimmed; a leading command character escaped
/// or refused per [`ScreenProfile::PREFIX`]. Returns (text, escaped, normalized).
pub(crate) fn deliverable<P: ScreenProfile>(text: &str) -> Result<(String, bool, bool)> {
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
        trimmed.len() <= P::MAX_TEXT && P::MAX_LINES.is_none_or(|m| trimmed.lines().count() <= m),
        "{}",
        P::too_long()
    );
    if let Prefix::Refuse(set, why) = P::PREFIX {
        ensure!(!trimmed.starts_with(set), "{why}");
    }
    ensure!(
        !trimmed.ends_with('\\'),
        "TERMINAL_PAYLOAD_UNSUPPORTED: a trailing backslash turns Enter into a newline"
    );
    let last = trimmed.split_whitespace().last().unwrap_or_default();
    ensure!(
        !last.starts_with('@') && !last.starts_with('/'),
        "TERMINAL_PAYLOAD_UNSUPPORTED: text ending in an @ or / token would submit a completion; add punctuation after it"
    );
    let escaped = matches!(P::PREFIX, Prefix::Escape(set) if trimmed.starts_with(set));
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

// ---------------------------------------------------------------- host

pub(crate) struct Host {
    session: String,
    pane: String,
    socket: Option<PathBuf>,
    /// HERDR's agent kind for this provider (`claude`, `gemini`, `agy`).
    agent: &'static str,
}
impl Host {
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
    /// Contract 1.63.0: the exact provider process (pid and start time) still runs and the
    /// pane's foreground is not a bare shell. A provider that exited (for example after a
    /// folder-trust restart that never came back) leaves the pane's shell in front, and typed
    /// keys would run there. `Err` is the refusal code; nothing was typed.
    pub(crate) fn provider_in_front(&self, pid: u32, start: u64) -> Result<(), &'static str> {
        if crate::external::attachment_definitely_gone(pid, start) {
            return Err("TERMINAL_PROVIDER_EXITED");
        }
        let info = crate::terminal::call_at(
            &self.session,
            &["pane", "process-info", "--pane", &self.pane],
            self.socket.as_deref(),
        )
        .map_err(|_| "TERMINAL_HOST_UNAVAILABLE")?;
        match crate::terminal::process_state(&info["process_info"]) {
            Ok((true, _)) => Err("TERMINAL_PANE_IS_SHELL"),
            _ => Ok(()),
        }
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
                tokio::time::sleep(crate::timing::scaled(CHUNK_PAUSE)).await;
            }
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(CHUNK) {
                self.text(&chunk.iter().collect::<String>())?;
                tokio::time::sleep(crate::timing::scaled(CHUNK_PAUSE)).await;
            }
        }
        Ok(())
    }
    /// Type text like [`Self::type_text`], reading the screen every `SEGMENT`
    /// non-blank characters. Claude and agy draw a long draft in a box of
    /// limited height that shows only its tail, so the whole text is never on
    /// screen at once. Each check requires the visible text to be a suffix of
    /// everything typed so far and at least as long as what was typed since the
    /// last check: consecutive windows then tile the text, and every character
    /// is seen on screen. Stops at the first failed check (false); the caller
    /// withdraws. `view` gives the box text of a main prompt, None otherwise.
    pub(crate) async fn type_checked(
        &self,
        s: &str,
        view: impl Fn(&[Vec<Cell>]) -> Option<String>,
    ) -> Result<bool> {
        let mut typed = String::new();
        let mut pending = 0usize;
        let lines: Vec<&str> = s.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                self.keys(&["ctrl+j"])?;
                tokio::time::sleep(crate::timing::scaled(CHUNK_PAUSE)).await;
            }
            let chars: Vec<char> = line.chars().collect();
            let chunks: Vec<&[char]> = chars.chunks(CHUNK).collect();
            for (j, chunk) in chunks.iter().enumerate() {
                let part: String = chunk.iter().collect();
                self.text(&part)?;
                tokio::time::sleep(crate::timing::scaled(CHUNK_PAUSE)).await;
                let sq = squash(&part);
                pending += sq.chars().count();
                typed.push_str(&sq);
                let last = i + 1 == lines.len() && j + 1 == chunks.len();
                if (pending >= SEGMENT || last) && pending > 0 {
                    if !self.window_ok(&typed, pending, &view).await? {
                        return Ok(false);
                    }
                    pending = 0;
                }
            }
        }
        Ok(true)
    }
    async fn window_ok(
        &self,
        typed: &str,
        pending: usize,
        view: &impl Fn(&[Vec<Cell>]) -> Option<String>,
    ) -> Result<bool> {
        let deadline = Instant::now() + SCREEN_WAIT;
        loop {
            tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
            if let Some(d) = view(&self.screen()?) {
                let w = squash(&d);
                if w.chars().count() >= pending && typed.ends_with(&w) {
                    return Ok(true);
                }
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
}

pub(crate) fn blocked(host: &Host) -> bool {
    matches!(host.agent_status(), Ok(Some(s)) if s == "blocked")
}
pub(crate) fn working(host: &Host) -> bool {
    matches!(host.agent_status(), Ok(Some(s)) if s == "working")
}
/// The code of a refusal text (`CODE: why`).
pub(crate) fn code(s: &str) -> &str {
    s.split(':').next().unwrap_or(s)
}

/// Read the screen until `want` holds or the wait ends; returns the last view.
pub(crate) async fn until<P: ScreenProfile>(
    host: &Host,
    strict: bool,
    want: impl Fn(&Screen) -> bool,
) -> Result<Screen> {
    let deadline = Instant::now() + SCREEN_WAIT;
    loop {
        tokio::time::sleep(crate::timing::scaled(SETTLE)).await;
        let v = P::classify(&host.screen()?, strict);
        if want(&v) || Instant::now() >= deadline {
            return Ok(v);
        }
    }
}

/// Clear the input box holding `text` with the profile's keys.
pub(crate) async fn clear<P: ScreenProfile>(host: &Host, text: &str) -> Result<()> {
    let c = &P::CLEAR;
    let rows = text
        .split('\n')
        .map(|l| l.chars().count() / c.row_chars + 1)
        .sum::<usize>();
    if !c.first.is_empty() {
        host.keys(c.first)?;
    }
    for _ in 0..(rows * c.rounds_per_row + c.extra).min(c.max) {
        host.keys(c.round)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- worker

/// Recheck the target before touching the pane: same catalog entry and live process,
/// still attached to the same HERDR pane. Returns the target, the account home and the
/// process observation.
pub(crate) fn recheck<P: ScreenProfile>(
    paths: &Paths,
    r: &Record,
) -> Result<(Terminal, PathBuf, crate::provider::claude::Observation)> {
    ensure!(terminal::enabled(paths), "{}", terminal::DISABLED);
    let account = crate::messaging::headless::usable_account(paths, P::PROVIDER, &r.account)?;
    let session = catalog::find(paths, &r.session_id)?;
    ensure!(session.conversation_id == r.conversation_id, "STALE_TARGET");
    P::before_observe(paths, r)?;
    let recorded = r.terminal.clone().context("STALE_TARGET")?;
    let o = P::observe_process(
        &account.home,
        &r.conversation_id,
        session.attachment.as_ref(),
    );
    let now = terminal::eligible(&o).map_err(|code| anyhow::anyhow!("{code}"))?;
    ensure!(
        now.pid == recorded.pid
            && now.process_start == recorded.process_start
            && now.target == recorded.target,
        "STALE_TARGET: the attached {} process or pane changed",
        P::PROCESS
    );
    Ok((
        Terminal {
            transcript: now.transcript.or(recorded.transcript.clone()),
            ..recorded
        },
        account.home,
        o,
    ))
}

/// A HERDR failure mid-procedure: never retried. Before Enter the request is unsent;
/// after it, the transcript decides. A draft whose restore was not verified stays in the
/// private journal.
pub(crate) fn worker_failed(paths: &Paths, id: &str, e: &anyhow::Error) -> Result<()> {
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
            r.observation_error = Some(format!("worker_error: {}", reason(e)));
        } else if r.phase == "admitted" {
            finish(r, "rejected", Some(&reason(e)));
        } else {
            finish(r, "rejected", Some("TERMINAL_HOST_FAILED_BEFORE_SUBMIT"));
        }
        Ok(())
    })
}

/// Steps 1-7 of the procedure for the record `id`, typing `t.delivered_text`.
pub(crate) async fn deliver<P: ScreenProfile>(
    p: &P,
    paths: &Paths,
    id: &str,
    host: &Host,
    t: &Terminal,
) -> Result<()> {
    let mode = if P::MODE_GATED {
        read(paths, id)?.request.mode
    } else {
        String::new()
    };
    // 1. HERDR agent state (read-only).
    match host.agent_status() {
        Ok(Some(s)) if s == "blocked" => return reject(paths, id, "TERMINAL_AGENT_BLOCKED"),
        Ok(Some(_)) => {}
        Ok(None) => return reject(paths, id, "TERMINAL_AGENT_UNKNOWN"),
        Err(_) => return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE"),
    }
    // Contract 1.63.0: the pane must run the exact provider process, not a shell.
    if let Err(code) = host.provider_in_front(t.pid, t.process_start) {
        return reject(paths, id, code);
    }
    // 2. View: the main prompt, or a view left with its own toggle.
    let Ok(screen) = host.screen() else {
        return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE");
    };
    let mut view = P::classify(&screen, true);
    for _ in 0..P::VIEW_SWITCHES {
        let Some((name, leave)) = view.as_ref().ok().and_then(P::leave) else {
            break;
        };
        step(paths, id, &format!("leave_{name}"), |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.view_switched = Some(name.into());
            }
        })?;
        match leave {
            Leave::Keys(keys) => host.keys(keys)?,
            Leave::Text(s) => host.text(s)?,
        }
        view = until::<P>(host, true, |v| {
            matches!(v, Ok(View::Main { .. })) || v.is_err()
        })
        .await?;
    }
    let (draft, busy_screen) = match view {
        Ok(View::Main { draft, busy }) => (draft, busy),
        Ok(_) => return reject(paths, id, "TERMINAL_VIEW_UNDETERMINED"),
        Err(code) if P::REFUSAL_LAYOUT => {
            return reject_screen(paths, id, code, &host.screen().unwrap_or(screen));
        }
        Err(code) => return reject(paths, id, code),
    };
    if !P::restorable(&draft) {
        // Typing it back would toggle help, a mode, a menu or a completion instead.
        return reject(paths, id, "TERMINAL_DRAFT_UNRESTORABLE");
    }
    if P::MODE_GATED {
        let busy = busy_screen || working(host);
        if let Some(refusal) = p.refusal(&mode, busy) {
            return reject(paths, id, code(refusal));
        }
    }
    // Re-check the agent just before the first destructive key.
    if blocked(host) {
        return reject(paths, id, "TERMINAL_AGENT_BLOCKED");
    }
    if P::REREAD_DRAFT {
        // Re-read the draft right before the first clearing key: text typed since
        // the first read would be destroyed by it. Nothing was sent yet.
        match host.screen() {
            Ok(rows) if is_main(&P::classify(&rows, true), |d| d == draft) => {}
            Ok(_) => return reject(paths, id, "TERMINAL_DRAFT_CHANGED"),
            Err(_) => return reject(paths, id, "TERMINAL_HOST_UNAVAILABLE"),
        }
    }
    // 3. Journal the draft, then clear. From here on every exit restores the
    //    draft or accounts for it.
    step(paths, id, "clear", |r| {
        r.phase = "dispatching".into();
        r.dispatched_at = Some(crate::daemon::now());
        if let Some(t) = r.terminal.as_mut() {
            t.draft = Some(draft.clone());
            t.draft_chars = Some(draft.chars().count());
        }
    })?;
    let mut submitted = false;
    let outcome = typed_input(p, paths, id, host, t, &mode, &draft, &mut submitted).await;
    if P::DRAFT == Draft::Reported
        && let Err(e) = &outcome
    {
        salvage::<P>(paths, id, host, &draft, &t.delivered_text, submitted, e).await;
        if submitted {
            change(paths, id, |r| {
                if !is_terminal(r) && !r.facts.written {
                    finish(r, "delivery_unknown", Some("TERMINAL_SUBMIT_UNCERTAIN"));
                }
                Ok(())
            })?;
        }
    }
    outcome
}

/// Steps 4-7 after the draft is journaled. `submitted` is set once the submit key went out
/// ([`Draft::Reported`]).
#[allow(clippy::too_many_arguments)]
async fn typed_input<P: ScreenProfile>(
    p: &P,
    paths: &Paths,
    id: &str,
    host: &Host,
    t: &Terminal,
    mode: &str,
    draft: &str,
    submitted: &mut bool,
) -> Result<()> {
    if !draft.is_empty() {
        clear::<P>(host, draft).await?;
        let v = until::<P>(host, true, is_empty).await?;
        if !is_empty(&v) {
            // Unknown input state: never type over it.
            return match P::DRAFT {
                Draft::Reported => {
                    left(
                        paths,
                        id,
                        "clear_unverified_draft_kept_in_journal",
                        &v,
                        draft,
                        "",
                    )?;
                    reject(paths, id, "TERMINAL_CLEAR_UNVERIFIED")
                }
                Draft::Kept => change(paths, id, |r| {
                    if let Some(t) = r.terminal.as_mut() {
                        t.draft_restored = Some(false);
                        t.restore_error = Some("clear_unverified_draft_kept_in_journal".into());
                    }
                    finish(r, "rejected", Some("TERMINAL_CLEAR_UNVERIFIED"));
                    Ok(())
                }),
            };
        }
    }
    // 4. Type the message and check it on screen.
    step(paths, id, "type", |_| {})?;
    let text = &t.delivered_text;
    let (ok, typed) = match P::TYPING {
        Typing::Checked { reread } => {
            let ok = host
                .type_checked(text, |rows| match P::classify(rows, false) {
                    Ok(View::Main { draft, .. }) => Some(draft),
                    _ => None,
                })
                .await?;
            if reread {
                let v = P::classify(&host.screen()?, false);
                (ok && is_main(&v, |_| true), Some(v))
            } else {
                (ok, None)
            }
        }
    };
    let blocked_once = P::ONE_BLOCKED_READ.then(|| blocked(host));
    if !ok || blocked_once.unwrap_or_else(|| blocked(host)) {
        let code = if blocked_once.unwrap_or_else(|| blocked(host)) {
            "TERMINAL_AGENT_BLOCKED"
        } else {
            "TERMINAL_TYPED_TEXT_MISMATCH"
        };
        return withdraw::<P>(paths, id, host, text, draft, code).await;
    }
    // 5. What the key does now: the mode must still fit.
    let (key, busy) = if P::MODE_GATED {
        let busy = matches!(typed, Some(Ok(View::Main { busy: true, .. }))) || working(host);
        match p.key(mode, busy) {
            Ok(k) => (k, busy),
            Err(refusal) => {
                return withdraw::<P>(paths, id, host, text, draft, code(refusal)).await;
            }
        }
    } else {
        (
            Key {
                key: "enter",
                expected: None,
            },
            false,
        )
    };
    // 6. Submit: journal first, then exactly one key.
    let anchor = p.anchor(paths, id, t)?;
    let at = now_ms() - 2000;
    step(paths, id, "submit", |r| {
        if let Some(t) = r.terminal.as_mut() {
            t.anchor_offset = anchor;
            t.submitted_at_ms = Some(at);
            p.submit_facts(t, busy, &key);
        }
    })?;
    // The screen read above came well after the last typed key, so Gemini's
    // 30 ms fast-return rule (Enter right after typing is a newline) is past.
    // Contract 1.67.0 (fix): a refusal here comes after typing, so the typed text is
    // withdrawn and the draft put back like every other post-typing refusal. When the
    // provider is gone (its shell is in front), keys would run in the shell instead:
    // nothing is sent and the draft stays in the journal.
    match host.provider_in_front(t.pid, t.process_start) {
        Ok(()) => {}
        Err(code @ ("TERMINAL_PANE_IS_SHELL" | "TERMINAL_PROVIDER_EXITED")) => {
            if !draft.is_empty() {
                unverified::<P>(
                    paths,
                    id,
                    "withdraw_unverified_draft_kept_in_journal",
                    &Err(code),
                    draft,
                    text,
                )?;
            }
            return reject(paths, id, code);
        }
        Err(code) => return withdraw::<P>(paths, id, host, text, draft, code).await,
    }
    let sent = host.keys(&[key.key]);
    match P::DRAFT {
        Draft::Reported => {
            *submitted = true;
            // Whether a failed Enter landed is unknown: the caller first salvages the
            // draft (only onto an input box seen empty), then ends the request
            // `delivery_unknown` (TERMINAL_SUBMIT_UNCERTAIN).
            sent?;
            change(paths, id, |r| {
                r.phase = "written".into();
                r.facts.written = true;
                Ok(())
            })?;
        }
        Draft::Kept => {
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
        }
    }
    // 7. The input box empties on submit (a turn, a queued message or a steer); a busy
    //    submit shows the provider's evidence. Then the draft goes back, unsubmitted.
    let empty = if P::DRAFT == Draft::Reported && draft.is_empty() {
        None
    } else {
        Some(until::<P>(host, true, is_empty).await?)
    };
    if busy {
        let lines: Vec<String> = host
            .screen()
            .map(|s| s.iter().map(|r| row_text(r)).collect())
            .unwrap_or_default();
        if let Some(e) = p.evidence(&lines, text) {
            change(paths, id, |r| {
                if let Some(t) = r.terminal.as_mut() {
                    P::record_evidence(t, e);
                }
                Ok(())
            })?;
            change(paths, id, |r| {
                if !is_terminal(r) && r.state == "pending_local" {
                    r.state = "native_accepted".into();
                }
                Ok(())
            })?;
        }
    }
    match empty {
        _ if draft.is_empty() => change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.draft = None;
            }
            Ok(())
        }),
        Some(v) if is_empty(&v) => restore::<P>(paths, id, host, draft, text).await,
        v => unverified::<P>(
            paths,
            id,
            "input_not_empty_after_submit",
            &v.unwrap_or(Err("TERMINAL_HOST_UNAVAILABLE")),
            draft,
            text,
        ),
    }
}

/// Record a draft whose restore is not verified (see [`Draft`]).
fn unverified<P: ScreenProfile>(
    paths: &Paths,
    id: &str,
    error: &str,
    v: &Screen,
    draft: &str,
    message: &str,
) -> Result<()> {
    match P::DRAFT {
        Draft::Reported => left(paths, id, error, v, draft, message),
        Draft::Kept => change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.draft_restored = Some(false);
                t.restore_error = Some(error.into());
            }
            Ok(())
        }),
    }
}

/// Withdraw typed text and put the draft back; then refuse with `code`.
pub(crate) async fn withdraw<P: ScreenProfile>(
    paths: &Paths,
    id: &str,
    host: &Host,
    typed: &str,
    draft: &str,
    code: &str,
) -> Result<()> {
    step(paths, id, "withdraw", |_| {})?;
    clear::<P>(host, typed).await?;
    let cleared = until::<P>(host, true, is_empty).await?;
    if is_empty(&cleared) {
        restore::<P>(paths, id, host, draft, typed).await?;
    } else {
        unverified::<P>(
            paths,
            id,
            "withdraw_unverified_draft_kept_in_journal",
            &cleared,
            draft,
            typed,
        )?;
    }
    reject(paths, id, code)
}

/// Type the draft back (no Enter) and verify it exactly; never retried. `message` is the
/// typed text, for reporting what the box holds ([`Draft::Reported`]).
pub(crate) async fn restore<P: ScreenProfile>(
    paths: &Paths,
    id: &str,
    host: &Host,
    draft: &str,
    message: &str,
) -> Result<()> {
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
    let read = until::<P>(host, true, |v| is_main(v, |d| d == draft)).await;
    let host_failed = read.is_err();
    let v = read.unwrap_or(Err("TERMINAL_HOST_UNAVAILABLE"));
    let ok = typed.is_ok() && is_main(&v, |d| d == draft);
    let error = match (&typed, &v) {
        (Err(_), _) => "restore_keys_failed".to_owned(),
        (_, Err(code)) if P::RESTORE_ERROR_NAMES_HOST || !host_failed => {
            format!("restore_unverified:{code}")
        }
        _ => "restore_unverified".to_owned(),
    };
    match P::DRAFT {
        Draft::Reported if ok => change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.draft_restored = Some(true);
                t.draft = None;
                t.left_in_input = Some("draft".into());
                t.left_chars = Some(draft.chars().count());
            }
            Ok(())
        }),
        Draft::Reported => left(paths, id, &error, &v, draft, message),
        Draft::Kept => change(paths, id, |r| {
            if let Some(t) = r.terminal.as_mut() {
                t.draft_restored = Some(ok);
                if ok {
                    t.draft = None;
                } else {
                    t.restore_error = Some(error.clone());
                }
            }
            Ok(())
        }),
    }
}

/// What the input box holds when a restore could not be verified.
pub(crate) fn left_in_input(
    v: &Screen,
    draft: &str,
    message: &str,
) -> (&'static str, Option<usize>) {
    match v {
        Ok(View::Main { draft: d, .. }) if d.is_empty() => ("empty", Some(0)),
        Ok(View::Main { draft: d, .. }) if d == draft => ("draft", Some(d.chars().count())),
        Ok(View::Main { draft: d, .. }) if !message.is_empty() && squash(d) == squash(message) => {
            ("message", Some(d.chars().count()))
        }
        Ok(View::Main { draft: d, .. }) => ("other", Some(d.chars().count())),
        _ => ("unknown", None),
    }
}

/// Record an unverified restore: the error, and what the input box holds.
/// The draft stays in the private journal.
pub(crate) fn left(
    paths: &Paths,
    id: &str,
    error: &str,
    v: &Screen,
    draft: &str,
    message: &str,
) -> Result<()> {
    let (what, chars) = left_in_input(v, draft, message);
    change(paths, id, |r| {
        if let Some(t) = r.terminal.as_mut() {
            t.draft_restored = Some(what == "draft");
            if what == "draft" {
                t.draft = None;
            } else {
                t.restore_error = Some(error.into());
            }
            t.left_in_input = Some(what.into());
            t.left_chars = chars;
        }
        Ok(())
    })
}

/// After a HERDR failure past the clear: one best-effort pass that puts the
/// draft back when the input box is seen empty (or still holds the unsent
/// message), else records what it holds. Never sends Enter.
async fn salvage<P: ScreenProfile>(
    paths: &Paths,
    id: &str,
    host: &Host,
    draft: &str,
    message: &str,
    submitted: bool,
    error: &anyhow::Error,
) {
    let done = read(paths, id)
        .ok()
        .and_then(|r| r.terminal)
        .is_some_and(|t| t.draft_restored.is_some());
    // An empty draft still needs the pass: the typed message itself may be sitting in the box.
    if done {
        return;
    }
    let note = format!("host_failed:{}", reason(error));
    let Ok(rows) = host.screen() else {
        let _ = left(
            paths,
            id,
            &note,
            &Err("TERMINAL_HOST_UNAVAILABLE"),
            draft,
            message,
        );
        return;
    };
    let mut v = P::classify(&rows, false);
    let unsent = !submitted && is_main(&v, |d| typed_remains(d, message));
    if unsent && clear::<P>(host, message).await.is_ok() {
        v = until::<P>(host, true, is_empty)
            .await
            .unwrap_or(Err("TERMINAL_HOST_UNAVAILABLE"));
    }
    if is_empty(&v) {
        if restore::<P>(paths, id, host, draft, message).await.is_err() {
            let _ = left(
                paths,
                id,
                &note,
                &Err("TERMINAL_HOST_UNAVAILABLE"),
                draft,
                message,
            );
        }
    } else {
        let _ = left(paths, id, &note, &v, draft, message);
    }
}

/// Whether `shown` (the input box as read) is what is left of the typed `message`: all of it,
/// or the tail of it while the box shows only its last rows, or what an interrupted typing
/// had written so far.
pub(crate) fn typed_remains(shown: &str, message: &str) -> bool {
    let s = squash(shown);
    let m = squash(message);
    !s.is_empty() && (s == m || (s.chars().count() >= 12 && (m.ends_with(&s) || m.starts_with(&s))))
}

/// After a submit: wait for the input record (or report the send uncertain), then follow
/// the reply until the turn ends or the timeout. The turn ends at the next real input in
/// the transcript, or when HERDR has reported the agent working and then idle for two
/// seconds.
pub(crate) async fn observe_loop<P: ScreenProfile>(
    paths: &Paths,
    id: &str,
    host: &Host,
) -> Result<()> {
    let r = read(paths, id)?;
    if is_terminal(&r) || !r.facts.written {
        return Ok(());
    }
    let timeout = r
        .request
        .timeout_seconds
        .unwrap_or(super::DEFAULT_TIMEOUT_SECONDS);
    let start = Instant::now();
    let confirm = P::confirm_window(&r);
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
        let settled = worked && idle_since.is_some_and(|s| s.elapsed() >= crate::timing::secs(2));
        P::apply_transcript(paths, id, settled)?;
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
        tokio::time::sleep(crate::timing::scaled(Duration::from_millis(250))).await;
    }
}

// ------------------------------------------------------- session capability

/// The per-session submit/queue/steer row of a typed-input-only provider (Gemini, agy).
pub(crate) trait SessionInput {
    /// The provider in refusal texts.
    const NAME: &'static str;
    const LABEL: &'static str;
    type Gates;
    /// The session's own settings that gate modes (read from its process and home).
    fn gates(value: &Value, home: Option<&Path>) -> Self::Gates;
    /// Why a mode is unavailable on a running, attached session; None when typed.
    fn mode_reason(gates: &Self::Gates, mode: &str) -> Option<String>;
    fn steer_semantics() -> Value;
    /// The provider's own key in `input_delivery`, last.
    fn row_extra(gates: Self::Gates) -> (&'static str, Value);
}

/// Per-session submit/queue/steer for a typed-input-only session row.
pub(crate) fn apply_session_input<P: SessionInput>(
    value: &mut Value,
    home: Option<&Path>,
    terminal_enabled: bool,
) {
    let running = value["availability"] == "running";
    let typed = super::terminal_reason(value, terminal_enabled);
    let gates = P::gates(value, home);
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
            m.2 = Some(format!(
                "SESSION_NOT_RUNNING: use mode resume (headless) for a stopped {} session",
                P::NAME
            ));
        } else if let Some(code) = typed {
            m.2 = Some(format!(
                "{code}: {} takes live input only as typed input into a verified HERDR pane",
                P::NAME
            ));
        } else {
            match P::mode_reason(&gates, m.0) {
                None => m.1 = Some(terminal::SEMANTICS),
                Some(reason) => m.2 = Some(reason),
            }
        }
    }
    for (k, path, reason) in &modes {
        value["capabilities"][*k] = json!(path.is_some());
        value["capability_reasons"][*k] = json!(reason);
    }
    let available = terminal_reason.is_none();
    let path = modes[0].1;
    let (key, extra) = P::row_extra(gates);
    value["input_delivery"] = json!({"semantics":path,"interrupt":false,
        "label":path.map(|_| P::LABEL),
        "steer":modes[2].1.map(|_| P::steer_semantics()),
        "modes":{"submit":modes[0].1,"queue":modes[1].1,"steer":modes[2].1},
        "available":if available {vec![terminal::SEMANTICS]} else {vec![]},
        "terminal_input":{"available":available,"reason":terminal_reason}});
    value["input_delivery"][key] = extra;
}

#[cfg(test)]
mod tests;
