//! Who is calling `api send`: contract 1.41.0 origin detection.
//!
//! A caller that omits `origin` is an agent when its own process tree contains the verified
//! process of a live sessionsation-managed session (catalog attachment pid and start time).
//! Operators (the HERDR UI, a plain shell) have no such ancestor and stay `operator`.

use crate::{
    catalog,
    paths::Paths,
    provider::{ProviderId, claude::native_peer::Sender},
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
        .find(|s| s.provider == "codex" && s.conversation_id == thread)
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

/// Walk the parents of `start` (inclusive) and report whether one is an attachment.
#[cfg(test)]
fn chain_hits(
    start: u32,
    attachments: &[(u32, u64)],
    stat: impl Fn(u32) -> Option<(u32, u64)>,
) -> bool {
    chain_match(start, attachments, stat).is_some()
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
    let found = catalog::list(paths).ok().and_then(|sessions| {
        let s = find_session(
            &sessions,
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
    });
    match found {
        Some(s) if !s.name.is_empty() => s,
        _ => Sender::default(),
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
    let bypass = match s.provider.as_str() {
        "claude" => s
            .attachment
            .as_ref()
            .and_then(|a| args_of(a.pid))
            .is_some_and(|args| claude_args_bypass(&args)),
        "codex" => codex_perm(s).is_some_and(|(a, sb)| a == "never" && sb == "danger-full-access"),
        _ => false,
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
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_the_chain_and_checks_start_time() {
        let table = |pid: u32| match pid {
            50 => Some((40, 500)),
            40 => Some((30, 400)),
            30 => Some((1, 300)),
            _ => None,
        };
        assert!(chain_hits(50, &[(30, 300)], table));
        // pid reuse: same pid, different start time.
        assert!(!chain_hits(50, &[(30, 999)], table));
        assert!(!chain_hits(50, &[(77, 300)], table));
        assert!(chain_hits(40, &[(40, 400)], table));
        assert!(!chain_hits(999, &[(30, 300)], table));
    }

    #[test]
    fn a_parent_loop_terminates() {
        assert!(!chain_hits(5, &[(9, 9)], |p| Some((p % 2 + 2, 1))));
    }

    #[test]
    fn explicit_origin_wins() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            resolve(&paths, Some("operator")),
            ("operator".into(), "explicit")
        );
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::{catalog::Attachment, provider::ProviderId};

    fn register(paths: &Paths, pid: u32, start: u64) {
        catalog::record_provider_launch(
            paths,
            ProviderId::Claude,
            "personal",
            &uuid::Uuid::new_v4().to_string(),
            "managed",
            None,
            Some(Attachment {
                host: "herdr/x".into(),
                target: "herdr/x/w:p".into(),
                pid,
                process_start: start,
            }),
        )
        .unwrap();
    }

    #[test]
    fn a_caller_inside_a_managed_session_is_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
        let me = std::process::id();
        let (_, start) = proc_stat(me).unwrap();
        // No attachments: a plain shell.
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
        // An unrelated live process is not an ancestor.
        register(&paths, 1, 1);
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
        // A recycled pid (same pid, other start time) does not count.
        register(&paths, me, start + 1);
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
        // This process (and so everything it runs) is inside the managed session.
        register(&paths, me, start);
        assert_eq!(resolve(&paths, None), ("agent".into(), "detected"));
        // An explicit origin always wins.
        assert_eq!(
            resolve(&paths, Some("operator")),
            ("operator".into(), "explicit")
        );
        assert_eq!(resolve(&paths, Some("agent")), ("agent".into(), "explicit"));
    }
}

#[cfg(test)]
mod codex_tests {
    use super::*;
    use crate::{catalog::Attachment, provider::ProviderId};

    fn table(pid: u32) -> Option<(u32, u64)> {
        match pid {
            50 => Some((40, 500)),
            40 => Some((30, 400)),
            30 => Some((1, 300)),
            _ => None,
        }
    }

    fn sessions(paths: &Paths) -> Vec<catalog::Session> {
        let att = |pid| Attachment {
            host: "h".into(),
            target: "h/t".into(),
            pid,
            process_start: 300,
        };
        let launch = |p, conv: &str, pid| {
            catalog::record_provider_launch(paths, p, "a", conv, "managed", None, Some(att(pid)))
                .unwrap();
        };
        launch(ProviderId::Claude, "claude-conv", 30);
        launch(ProviderId::Codex, "thread-1", 7777);
        catalog::list(paths).unwrap()
    }

    fn name_of(s: Option<&catalog::Session>) -> Option<String> {
        s.map(|s| s.name.clone())
    }

    #[test]
    fn ancestry_hit_wins_over_codex_thread() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
        let ss = sessions(&paths);
        let claude = ss.iter().find(|s| s.provider == "claude").unwrap();
        let hit = find_session(&ss, 50, table, Some("thread-1".into()), |_| {
            Some("thread-1".into())
        });
        assert_eq!(name_of(hit), Some(claude.name.clone()));
    }

    #[test]
    fn codex_thread_via_own_env_or_ancestor_after_env_i() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
        let ss = sessions(&paths);
        let codex = ss.iter().find(|s| s.provider == "codex").unwrap();
        let chain = |p: u32| if p == 50 { Some((40, 1)) } else { Some((1, 1)) };
        // Own env.
        let hit = find_session(&ss, 50, chain, Some("thread-1".into()), |_| None);
        assert_eq!(name_of(hit), Some(codex.name.clone()));
        // env -i: own env empty, the thread id lives on an ancestor.
        let hit = find_session(&ss, 50, chain, None, |p| {
            (p == 40).then(|| "thread-1".to_owned())
        });
        assert_eq!(name_of(hit), Some(codex.name.clone()));
        // A thread id equal to a non-codex conversation id does not match.
        assert!(find_session(&ss, 50, chain, None, |_| Some("claude-conv".into())).is_none());
    }

    #[test]
    fn unknown_thread_is_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
        let ss = sessions(&paths);
        let chain = |_: u32| Some((1, 1));
        assert!(find_session(&ss, 50, chain, Some("nope".into()), |_| None).is_none());
        assert!(find_session(&ss, 50, chain, None, |_| Some("nope".into())).is_none());
        assert!(find_session(&ss, 50, chain, None, |_| None).is_none());
        assert!(chain_thread(5, |p| Some((p % 2 + 2, 1)), |_| None).is_none());
    }

    #[test]
    fn environ_parsing_returns_only_the_thread_id() {
        let raw = b"A=1\0CODEX_THREAD_ID=abc-123\0CODEX_THREAD_ID_X=no\0SECRET=s\0";
        assert_eq!(thread_from_environ(raw), Some("abc-123".into()));
        assert_eq!(thread_from_environ(b"A=1\0CODEX_THREAD_ID=\0"), None);
        assert_eq!(thread_from_environ(b"A=1\0"), None);
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    fn session(provider: &str) -> catalog::Session {
        catalog::Session {
            id: "i".into(),
            name: "n".into(),
            provider: provider.into(),
            account: "a".into(),
            conversation_id: "c".into(),
            revision: 1,
            attachment: Some(catalog::Attachment {
                host: "h".into(),
                target: "t".into(),
                pid: 9,
                process_start: 1,
            }),
            directory: None,
            previous_conversation_ids: vec![],
            space: None,
        }
    }
    fn args(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn claude_mode_follows_its_own_launch_flags() {
        let s = session("claude");
        let mode = |v: &'static [&'static str]| session_mode(&s, move |_| args(v), |_| None);
        assert_eq!(
            mode(&["claude", "--dangerously-skip-permissions"]),
            "bypass"
        );
        assert_eq!(
            mode(&["claude", "--permission-mode", "bypassPermissions"]),
            "bypass"
        );
        assert_eq!(
            mode(&["claude", "--permission-mode=bypassPermissions"]),
            "bypass"
        );
        assert_eq!(mode(&["claude", "--permission-mode", "plan"]), "prompting");
        assert_eq!(mode(&["claude"]), "prompting");
        assert_eq!(session_mode(&s, |_| None, |_| None), "prompting");
    }

    #[test]
    fn codex_mode_needs_never_and_full_access() {
        let s = session("codex");
        let mode = |a: &str, sb: &str| {
            let p = Some((a.to_owned(), sb.to_owned()));
            session_mode(&s, |_| None, move |_| p.clone())
        };
        assert_eq!(mode("never", "danger-full-access"), "bypass");
        assert_eq!(mode("never", "workspace-write"), "prompting");
        assert_eq!(mode("on-request", "danger-full-access"), "prompting");
        assert_eq!(session_mode(&s, |_| None, |_| None), "prompting");
        // Another provider's flags never count.
        assert_eq!(
            session_mode(
                &session("gemini"),
                |_| args(&["--dangerously-skip-permissions"]),
                |_| None
            ),
            "prompting"
        );
    }
}
