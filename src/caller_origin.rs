//! Who is calling `api send`: contract 1.41.0 origin detection.
//!
//! A caller that omits `origin` is an agent when its own process tree contains the verified
//! process of a live sessionsation-managed session (catalog attachment pid and start time).
//! Operators (the HERDR UI, a plain shell) have no such ancestor and stay `operator`.

use crate::{
    catalog,
    paths::Paths,
    provider::{
        ProviderId,
        claude::native_peer::{self, Sender},
    },
};

/// How many parents are followed before giving up (guards against pid-reuse loops).
const MAX_HOPS: usize = 64;

/// `(origin, origin_source)`; the source is `explicit`, `detected` or `default`.
pub fn resolve(paths: &Paths, explicit: Option<&str>) -> (String, &'static str) {
    if let Some(origin) = explicit {
        return (origin.to_owned(), "explicit");
    }
    if inside_managed_session(paths) {
        ("agent".to_owned(), "detected")
    } else {
        ("operator".to_owned(), "default")
    }
}

fn inside_managed_session(paths: &Paths) -> bool {
    let Ok(sessions) = catalog::list(paths) else {
        return false;
    };
    find_session(
        &sessions,
        std::process::id(),
        proc_stat,
        std::env::var(CODEX_THREAD_VAR).ok(),
        proc_codex_thread,
    )
    .is_some()
}

/// The managed session a process runs under: its ancestry reaches a catalog attachment, or (Codex
/// tool commands are children of the daemon-owned app-server, not of the TUI attachment) it carries
/// the `CODEX_THREAD_ID` of a catalog `codex` session.
fn find_session(
    sessions: &[catalog::Session],
    start: u32,
    stat: impl Fn(u32) -> Option<(u32, u64)>,
    own_thread: Option<String>,
    thread_of: impl Fn(u32) -> Option<String>,
) -> Option<&catalog::Session> {
    let attachments: Vec<(u32, u64)> = sessions
        .iter()
        .filter_map(|s| s.attachment.as_ref())
        .map(|a| (a.pid, a.process_start))
        .collect();
    if let Some(hit) = chain_match(start, &attachments, &stat) {
        return sessions.iter().find(|s| {
            s.attachment
                .as_ref()
                .is_some_and(|a| (a.pid, a.process_start) == hit)
        });
    }
    let thread = own_thread
        .filter(|t| !t.is_empty())
        .or_else(|| chain_thread(start, &stat, &thread_of))?;
    sessions
        .iter()
        .find(|s| crate::provider::ops::supervised(&s.provider) && s.conversation_id == thread)
}

/// `CODEX_THREAD_ID` of the first process walking up from `start` (inclusive) that has one.
fn chain_thread(
    start: u32,
    stat: impl Fn(u32) -> Option<(u32, u64)>,
    thread_of: impl Fn(u32) -> Option<String>,
) -> Option<String> {
    let mut pid = start;
    for _ in 0..MAX_HOPS {
        if pid <= 1 {
            return None;
        }
        if let Some(t) = thread_of(pid).filter(|t| !t.is_empty()) {
            return Some(t);
        }
        pid = stat(pid)?.0;
    }
    None
}

/// The first attachment found walking up from `start`.
fn chain_match(
    start: u32,
    attachments: &[(u32, u64)],
    stat: impl Fn(u32) -> Option<(u32, u64)>,
) -> Option<(u32, u64)> {
    let mut pid = start;
    for _ in 0..MAX_HOPS {
        if pid <= 1 {
            return None;
        }
        let (parent, started) = stat(pid)?;
        if attachments.contains(&(pid, started)) {
            return Some((pid, started));
        }
        pid = parent;
    }
    None
}

/// Catalog name of the managed session this process runs under (a parent is its attachment);
/// `sessionsation` when called from outside any session (operator, supervisor, system).
pub fn sender_name(paths: &Paths) -> String {
    sender(paths).display_name().to_owned()
}

/// The sender of a native message: the catalog session this process runs under (name and the
/// session's own permission class), or the default `sessionsation` / `prompting` for an
/// operator, supervisor or unknown caller. Never the receiver's mode.
pub fn sender(paths: &Paths) -> Sender {
    managed_sender(paths).unwrap_or_default()
}

/// The managed session this process runs under, as a sender; `None` outside any session.
fn managed_sender(paths: &Paths) -> Option<Sender> {
    managed_in(paths, &catalog::list(paths).ok()?)
}

fn managed_in(paths: &Paths, sessions: &[catalog::Session]) -> Option<Sender> {
    let s = find_session(
        sessions,
        std::process::id(),
        proc_stat,
        std::env::var(CODEX_THREAD_VAR).ok(),
        proc_codex_thread,
    )?;
    let mode = session_mode(s, proc_args, |s| codex_permissions(paths, s));
    Some(Sender {
        name: s.name.clone(),
        mode: mode.into(),
        canonical: s.canonical(),
    })
    .filter(|s| !s.name.is_empty())
}

/// Facts value when a request's `from_component` was not used because the caller is a session.
pub const COMPONENT_IGNORED_SESSION: &str = "caller_is_session";

/// Facts value when a component was rendered `sessionsation:<component>` because its bare
/// form would collide with a session name or `sessionsation` (contract 1.51.0).
pub const COMPONENT_PREFIXED_COLLISION: &str = "collides_with_session";

/// The sender chosen at admission, plus the receipt facts explaining it.
#[derive(Debug, PartialEq)]
pub struct SenderChoice {
    pub sender: Sender,
    /// [`COMPONENT_IGNORED_SESSION`] when a request field was given by a session caller.
    pub ignored: Option<&'static str>,
    /// [`COMPONENT_PREFIXED_COLLISION`] when a request field fell back to the prefixed form.
    pub prefixed: Option<&'static str>,
}

/// [`sender`] naming the component behind an outside-session caller: the request's
/// `from_component`, else `env` (the `SESSIONSATION_SENDER_COMPONENT` of an `api send`
/// process; an invalid value is ignored with a stderr warning). Contract 1.51.0 renders the
/// bare component, unless it equals (case-insensitively) the name of any catalog session, in
/// any space, or `sessionsation`; it then falls back to `sessionsation:<component>` (never
/// refused). A catalog that cannot be read counts as a collision (fail safe). A
/// managed-session caller keeps its own name (no spoofing).
pub fn sender_for(paths: &Paths, field: Option<&str>, env: Option<&str>) -> SenderChoice {
    let sessions = catalog::list(paths).ok();
    let managed = sessions.as_deref().and_then(|s| managed_in(paths, s));
    let names = sessions.map(|s| s.into_iter().map(|s| s.name).collect::<Vec<_>>());
    choose_sender(managed, names.as_deref(), field, env, |m| {
        eprintln!("sessionsation: warning: {m}")
    })
}

/// Whether the bare `component` could be mistaken for a session (or Sessionsation itself).
/// `names` is `None` when the catalog could not be read: then always a collision.
fn collides(component: &str, names: Option<&[String]>) -> bool {
    // The catalog's own name folding (Unicode lowercase), so a component matches exactly
    // the names a session could not take next to that one.
    let fold = catalog::scope::fold;
    let c = fold(component);
    c == native_peer::DEFAULT_SENDER || names.is_none_or(|names| names.iter().any(|n| fold(n) == c))
}

fn choose_sender(
    managed: Option<Sender>,
    names: Option<&[String]>,
    field: Option<&str>,
    env: Option<&str>,
    mut warn: impl FnMut(String),
) -> SenderChoice {
    let component = match field {
        Some(f) => Some(f).filter(|f| native_peer::valid_component(f)),
        None => match env.filter(|e| !e.is_empty()) {
            Some(e) if native_peer::valid_component(e) => Some(e),
            Some(e) => {
                warn(format!(
                    "ignoring {}={e:?}: a component must match ^[A-Za-z0-9][A-Za-z0-9._-]{{0,39}}$",
                    native_peer::COMPONENT_ENV
                ));
                None
            }
            None => None,
        },
    };
    let (sender, ignored, prefixed) = match (managed, component) {
        (Some(s), _) => (
            s,
            field.is_some().then_some(COMPONENT_IGNORED_SESSION),
            None,
        ),
        (None, Some(c)) if collides(c, names) => {
            if field.is_none() {
                warn(format!(
                    "{}={c:?} collides with a session name or \"sessionsation\" (or the catalog is unreadable); sending as \"sessionsation:{c}\"",
                    native_peer::COMPONENT_ENV
                ));
            }
            (
                Sender::prefixed_component(c),
                None,
                field.is_some().then_some(COMPONENT_PREFIXED_COLLISION),
            )
        }
        (None, Some(c)) => (Sender::component(c), None, None),
        (None, None) => (Sender::default(), None, None),
    };
    SenderChoice {
        sender,
        ignored,
        prefixed,
    }
}

/// `bypass` only for a session known to run without permission prompts, else `prompting`.
/// Claude: its attachment process (verified launch) carries `--dangerously-skip-permissions` or
/// `--permission-mode bypassPermissions`. Codex: the latest recorded `turn_context` of its
/// rollout is approval `never` with sandbox `danger-full-access`.
fn session_mode(
    s: &catalog::Session,
    args_of: impl Fn(u32) -> Option<Vec<String>>,
    codex_perm: impl Fn(&catalog::Session) -> Option<(String, String)>,
) -> &'static str {
    let facts = crate::provider::ops::facts_of(&s.provider);
    let bypass = if facts.is_some_and(|f| f.bypass_warning) {
        s.attachment
            .as_ref()
            .and_then(|a| args_of(a.pid))
            .is_some_and(|args| claude_args_bypass(&args))
    } else if facts.is_some_and(|f| f.supervised) {
        codex_perm(s).is_some_and(|(a, sb)| a == "never" && sb == "danger-full-access")
    } else {
        false
    };
    if bypass { "bypass" } else { "prompting" }
}

fn claude_args_bypass(args: &[String]) -> bool {
    args.iter().any(|a| a == crate::provider::claude::YOLO_FLAG)
        || args
            .iter()
            .any(|a| a == "--permission-mode=bypassPermissions")
        || args
            .windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions")
}

fn proc_args(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect(),
    )
}

fn codex_permissions(paths: &Paths, s: &catalog::Session) -> Option<(String, String)> {
    let account =
        crate::messaging::headless::usable_account(paths, ProviderId::Codex, &s.account).ok()?;
    let rollout = crate::external::recovery_rollout(&account.home, &s.conversation_id).ok()??;
    crate::external::recovery_permissions(&rollout)
}

const CODEX_THREAD_VAR: &str = "CODEX_THREAD_ID";

/// `CODEX_THREAD_ID` from /proc/<pid>/environ. Only that one variable is read out; nothing else
/// from the environment is kept, logged or returned.
fn proc_codex_thread(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    thread_from_environ(&raw)
}

fn thread_from_environ(raw: &[u8]) -> Option<String> {
    let prefix = format!("{CODEX_THREAD_VAR}=");
    raw.split(|b| *b == 0)
        .find_map(|e| e.strip_prefix(prefix.as_bytes()))
        .and_then(|v| std::str::from_utf8(v).ok())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// `(ppid, start ticks)` from /proc.
fn proc_stat(pid: u32) -> Option<(u32, u64)> {
    crate::proc::identity::stat(pid)
        .ok()
        .map(|s| (s.parent, s.start_time))
}

#[cfg(test)]
mod tests;
#[cfg(test)]
use tests::chain_hits;
